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

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/**
 * Covers [SessionTabKeys]: the pending tab choice, and the sticky record of what tab a host is
 * on.
 *
 * These exist as a separate class because the distinction between them was got wrong and the
 * failure was invisible: the app's own reconnect built a new transport, the pending key had
 * already been consumed by the old one, and the session ended with "no tab was chosen" for a
 * tab the user had tapped. Nothing about that needs a socket to test — only the two maps and
 * the order they are asked in.
 */
class SessionTabKeysTest {

    private val hostId = 42L
    private val keys = SessionTabKeys()

    /**
     * A fresh connection takes the choice, once.
     *
     * The second take must be null: a choice belongs to one connection attempt, and a stale
     * one would send a later connection to a tab the user did not pick for it.
     */
    @Test
    fun aChoiceIsTakenExactlyOnce() {
        keys.choose(hostId, "tab-a")

        assertEquals("tab-a", keys.takePending(hostId))
        assertNull(keys.takePending(hostId))
    }

    /**
     * The tab survives being taken, which is the whole point.
     *
     * This is the regression test for the reported bug. A reconnect has no pending choice —
     * the old transport consumed it — so if choosing a tab did not also record it as the
     * host's current tab, `resumeKey` would return null and the reconnect would have no tab to
     * open, failing with "no tab was chosen" for a tab the user had tapped.
     *
     * The recording happens in [SessionTabKeys.choose], not in [SessionTabKeys.takePending];
     * see the comment there for why writing it in both places was worse than either.
     */
    @Test
    fun choosingRecordsTheTabAReconnectWouldOpen() {
        keys.choose(hostId, "tab-a")

        keys.takePending(hostId)

        assertEquals("tab-a", keys.resumeKey(hostId))
    }

    /**
     * A reconnect after a *switched* session lands on the tab that was switched to.
     *
     * A switch does not go through a new connection at all: the live session is told directly,
     * and `remember` is what records the move. Without it, a later reconnect would reopen the
     * tab the session started on, which is a different place entirely.
     */
    @Test
    fun switchingUpdatesTheTabAReconnectWouldOpen() {
        keys.choose(hostId, "tab-a")
        keys.takePending(hostId)
        keys.remember(hostId, "tab-b")

        assertEquals("tab-b", keys.resumeKey(hostId))
    }

    /**
     * Dropping a leftover pending choice keeps the host's current tab.
     *
     * The case: a tap queued a choice, no session was ever opened for it, and a live session
     * was then told to switch directly. The leftover choice belongs to nobody, but the switch
     * that just happened is the host's current tab and must survive.
     */
    @Test
    fun droppingAPendingChoiceKeepsTheCurrentTab() {
        keys.choose(hostId, "tab-a")
        keys.remember(hostId, "tab-b")

        keys.forgetPending(hostId)

        assertNull("the leftover choice is gone", keys.takePending(hostId))
        assertEquals("tab-b", keys.resumeKey(hostId))
    }

    /**
     * Hosts do not share state.
     *
     * Obvious, and worth one assertion: the keys are per host, and a mistake that made them a
     * single pair of fields would send one server's session to another server's tab.
     */
    @Test
    fun hostsAreIndependent() {
        keys.choose(1L, "tab-a")
        keys.choose(2L, "tab-b")

        assertEquals("tab-a", keys.takePending(1L))
        assertEquals("tab-b", keys.takePending(2L))
        assertEquals("tab-a", keys.resumeKey(1L))
        assertEquals("tab-b", keys.resumeKey(2L))
    }

    /**
     * Forgetting a host removes both halves.
     *
     * A closed session that left its tab behind would make the next connection to that host
     * open a tab from a session the user had ended.
     */
    @Test
    fun forgettingRemovesBothHalves() {
        keys.choose(hostId, "tab-a")

        keys.forget(hostId)

        assertNull(keys.takePending(hostId))
        assertNull(keys.resumeKey(hostId))
    }
}
