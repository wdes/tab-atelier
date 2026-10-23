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
import org.junit.Test

/**
 * Covers [ConnectionEventListener.upgradeOutcome], the one decision in the OkHttp listener that
 * is not simply a passthrough.
 *
 * It is a pure function on purpose: the case that used to be wrong is HTTP **101**, which
 * cannot be produced in a test without a live daemon performing a real WebSocket upgrade. A
 * rule that can only be checked by hand is a rule that will be checked once.
 */
class ConnectionEventListenerTest {

    /**
     * A 101 is the upgrade succeeding, not a server refusing.
     *
     * The old code announced any status outside `200..299` as a failure — and 101 is outside
     * `200..299`. So a *successful* WebSocket upgrade was drawn on the strip as
     * "Server answered 101 Switching Protocols", which reads as an error at the exact moment
     * everything worked.
     */
    @Test
    fun aSuccessfulUpgradeIsNotAnError() {
        assertEquals(
            ConnectionEventListener.UpgradeOutcome.SUCCESS,
            ConnectionEventListener.upgradeOutcome(101),
        )
    }

    /**
     * A refusal is a refusal, and says so.
     *
     * 401 and 404 are the ones a tab-atelier daemon actually produces: a bad token, or a tab
     * that no longer exists. Neither is a network problem, and the strip should not suggest
     * that it is.
     */
    @Test
    fun aRefusalIsReportedAsARefusal() {
        for (status in listOf(400, 401, 403, 404, 500, 502, 503)) {
            assertEquals(
                "status $status should be a refusal",
                ConnectionEventListener.UpgradeOutcome.REFUSED,
                ConnectionEventListener.upgradeOutcome(status),
            )
        }
    }

    /**
     * A 2xx is neither an upgrade nor a refusal.
     *
     * A plain 200 to an upgrade request means something in front of the daemon answered
     * instead of upgrading it — a login page, most likely. It is not an error to announce
     * either: the socket fails immediately afterwards and says so properly.
     */
    @Test
    fun aPlainSuccessIsNotAnUpgrade() {
        for (status in listOf(200, 204, 299)) {
            assertEquals(
                "status $status is not an upgrade",
                ConnectionEventListener.UpgradeOutcome.IGNORED,
                ConnectionEventListener.upgradeOutcome(status),
            )
        }
    }
}
