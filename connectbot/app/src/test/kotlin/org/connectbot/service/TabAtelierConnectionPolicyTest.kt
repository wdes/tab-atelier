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

package org.connectbot.service

import org.connectbot.service.TabAtelierConnectionPolicy.Companion.IDLE_TIMEOUT_MILLIS
import org.connectbot.service.TabAtelierConnectionPolicy.Companion.shouldSuspend
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Covers the rule that decides when a tab-atelier session lets go of its socket.
 *
 * Asserted as a pure function of the clock and the network so that the timing can be
 * checked without waiting ten real minutes or toggling a radio, which is the only way this
 * would otherwise be tested — that is to say, not at all.
 */
class TabAtelierConnectionPolicyTest {

    private val started = 1_000_000L

    /**
     * Nothing happens before the timeout.
     *
     * The bug this guards is a session that drops while the user is reading it: an
     * off-by-one in the comparison, or a timeout measured from the wrong instant, would
     * suspend a session that had just been used.
     */
    @Test
    fun anUntouchedSessionIsNotSuspendedBeforeTheTimeout() {
        assertFalse(
            shouldSuspend(
                now = started + IDLE_TIMEOUT_MILLIS - 1,
                lastInteractionAt = started,
                hasNetwork = true,
            ),
        )
    }

    /**
     * At exactly the timeout it suspends.
     *
     * `>=` and not `>`: ten minutes is ten minutes. An exclusive comparison here would be
     * invisible until somebody timed it.
     */
    @Test
    fun aSessionIsSuspendedAtTheTimeout() {
        assertTrue(
            shouldSuspend(
                now = started + IDLE_TIMEOUT_MILLIS,
                lastInteractionAt = started,
                hasNetwork = true,
            ),
        )
    }

    /**
     * Losing the network suspends immediately, however recently the user touched it.
     *
     * There is nothing to keep alive over a network that is not there, and every attempt to
     * keep it is a radio wake-up that fails — which is the battery drain the timeout exists
     * to avoid, arriving by a different route. Airplane mode lands here and needs no case
     * of its own.
     */
    @Test
    fun losingTheNetworkSuspendsImmediately() {
        assertTrue(
            shouldSuspend(
                now = started,
                lastInteractionAt = started,
                hasNetwork = false,
            ),
        )
    }

    /**
     * Use resets the clock.
     *
     * Asserted as "the timeout is measured from the last interaction", because the failure
     * mode is a timer that started once at launch: it would park the session of a user who
     * has been typing continuously for eleven minutes.
     */
    @Test
    fun theTimeoutIsMeasuredFromTheLastInteraction() {
        val touchedRecently = started + IDLE_TIMEOUT_MILLIS * 5

        assertFalse(
            shouldSuspend(
                now = touchedRecently + IDLE_TIMEOUT_MILLIS - 1,
                lastInteractionAt = touchedRecently,
                hasNetwork = true,
            ),
        )
    }
}
