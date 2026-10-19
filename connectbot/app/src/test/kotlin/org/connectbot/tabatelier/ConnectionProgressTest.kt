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
 * Covers [ConnectionProgress]: the one-line description of a connection attempt that the
 * console's status strip displays.
 *
 * The ticker is not started here — `tick()` is called directly — because what matters is the
 * line, and a test that waits real seconds for a dot is a test that goes flaky exactly when it
 * is needed.
 */
class ConnectionProgressTest {

    /** Every value the strip was asked to show, in order. */
    private val shown = mutableListOf<String?>()

    private fun progress() = ConnectionProgress(render = { shown += it })

    private val current: String? get() = shown.lastOrNull()

    /**
     * A phase is named, and each tick extends the same line by one full stop.
     *
     * The whole visible behaviour in one assertion — and the failure it guards is the one an
     * append-only API would produce: a fresh call per tick that replaces instead of extends,
     * so the strip would show exactly one dot at all times and never look like progress.
     */
    @Test
    fun aPhaseGainsOneDotPerTick() {
        val progress = progress()

        progress.phase("DNS lookup for home.example")
        progress.tick()
        progress.tick()
        progress.tick()

        assertEquals("DNS lookup for home.example...", current)
    }

    /**
     * A new phase replaces the previous one instead of joining it.
     *
     * A bar has one line. Joining would give "DNS lookup for …..TCP connection to …", which is
     * exactly the unreadable accumulation the strip exists to avoid.
     */
    @Test
    fun aNewPhaseReplacesThePreviousOne() {
        val progress = progress()

        progress.phase("DNS lookup for home.example")
        progress.tick()
        progress.tick()
        progress.phase("TCP connection to 192.168.1.10:8443")
        progress.tick()

        assertEquals("TCP connection to 192.168.1.10:8443.", current)
    }

    /**
     * Ticks after [ConnectionProgress.finish] change nothing.
     *
     * `finish` and the ticker are not atomic with each other: a tick already in flight must
     * not extend a line that describes a phase which has ended.
     */
    @Test
    fun ticksAfterFinishChangeNothing() {
        val progress = progress()

        progress.phase("TLS handshake with home.example")
        progress.finish()
        val afterFinish = shown.size
        progress.tick()
        progress.tick()

        assertEquals("finished phase must not keep growing", afterFinish, shown.size)
    }

    /**
     * A tick before any phase shows nothing.
     *
     * The ticker starts with the object, so its first tick can arrive before a phase is named
     * — and a bare dot appearing from nowhere is the first thing the user would see.
     */
    @Test
    fun aTickBeforeAnyPhaseShowsNothing() {
        val progress = progress()

        progress.tick()

        assertEquals(emptyList<String?>(), shown)
    }

    /**
     * Stopping clears the strip.
     *
     * The line describes a connection that is no longer being attempted; leaving it on screen
     * would be a permanent bar saying "TCP connection to …" over a session that connected
     * minutes ago. This is the assertion that keeps the strip from becoming the old always-on
     * "Connecting to" line by another name.
     */
    @Test
    fun stoppingClearsTheStrip() {
        val progress = progress()

        progress.phase("DNS lookup for home.example")
        progress.tick()
        progress.stop()

        assertNull(current)
    }

    /**
     * Stopping twice does not re-clear, and is safe without a start.
     *
     * The dial's `finally` always stops, including on paths where nothing was ever started,
     * and a second `render(null)` is at best noise.
     */
    @Test
    fun stoppingTwiceIsHarmless() {
        val progress = progress()

        progress.start()
        progress.stop()
        val afterFirstStop = shown.size
        progress.stop()

        assertEquals(afterFirstStop, shown.size)
    }
}
