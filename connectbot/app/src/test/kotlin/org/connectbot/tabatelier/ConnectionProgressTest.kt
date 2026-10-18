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
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Covers [ConnectionProgress]: the phase lines and their per-second dots.
 *
 * The ticker is not started here — `tick()` is called directly — because the point of the
 * class is what it writes, and a test that waits real seconds to see a dot is a test that will
 * be flaky at exactly the moment it matters.
 */
class ConnectionProgressTest {

    private val written = StringBuilder()
    private val joined: String get() = written.toString()

    private fun progress() = ConnectionProgress(write = { written.append(it) })

    /**
     * A phase is written as a line, and each tick adds exactly one dot.
     *
     * This is the whole visible behaviour, in one assertion: the phase names itself, and one
     * dot per tick appears on that same line — the failure mode of a misplaced newline being
     * every dot on a line of its own.
     */
    @Test
    fun aPhaseGainsOneDotPerTick() {
        val progress = progress()

        progress.phase("DNS lookup for home.example")
        progress.tick()
        progress.tick()
        progress.tick()

        assertEquals("DNS lookup for home.example...", joined)
    }

    /**
     * A second phase starts on a new line and does not inherit the first one's dots.
     *
     * Both halves matter. Without the newline the phases run together into one
     * unreadable line; without the reset, a phase inherits the previous phase's dots and
     * appears to have been slow when it was not.
     */
    @Test
    fun aNewPhaseStartsOnItsOwnLineWithNoDots() {
        val progress = progress()

        progress.phase("DNS lookup for home.example")
        progress.tick()
        progress.tick()
        progress.phase("TCP connection to 192.168.1.10:8443")
        progress.tick()

        assertEquals(
            "DNS lookup for home.example..\r\nTCP connection to 192.168.1.10:8443.",
            joined,
        )
    }

    /**
     * Ticks after [ConnectionProgress.finish] write nothing.
     *
     * The ticker is stopped by the caller as well, but the two are not atomic with each
     * other: a tick already in flight when `finish` lands must not put a dot on the line the
     * next phase — or the remote program — is about to write.
     */
    @Test
    fun ticksAfterFinishWriteNothing() {
        val progress = progress()

        progress.phase("TLS handshake with home.example")
        progress.finish()
        progress.tick()
        progress.tick()

        assertEquals("TLS handshake with home.example\r\n", joined)
    }

    /**
     * A tick before any phase writes nothing.
     *
     * The ticker starts with the object, so its first tick can arrive before the first phase
     * is named — and a bare dot on an otherwise empty terminal would be the first thing the
     * user saw.
     */
    @Test
    fun aTickBeforeAnyPhaseWritesNothing() {
        val progress = progress()

        progress.tick()

        assertEquals("", joined)
    }

    /**
     * `finish` twice is harmless.
     *
     * It is called on every exit path from the dial, including ones where the phase already
     * ended, and a second newline would leave a blank line on the terminal.
     */
    @Test
    fun finishingTwiceWritesOneNewline() {
        val progress = progress()

        progress.phase("DNS lookup for home.example")
        progress.finish()
        progress.finish()

        assertEquals("DNS lookup for home.example\r\n", joined)
    }

    /**
     * [ConnectionProgress.stop] is safe without [ConnectionProgress.start], and twice.
     *
     * The dial's `finally` block always stops, including when the progress was never started
     * — a null reporter, or a test — and a `Timer` cancelled twice throws nothing but a
     * missing one is a different matter. Cheap to get right, so it is asserted rather than
     * assumed.
     */
    @Test
    fun stopIsSafeWithoutStartAndTwice() {
        val progress = progress()

        progress.stop()
        progress.start()
        progress.stop()
        progress.stop()

        assertTrue(true)
    }
}
