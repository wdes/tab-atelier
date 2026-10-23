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
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Covers [ConnectionProgress]: the one-line description of a connection attempt that the
 * console's status strip displays.
 *
 * The ticker is not started here — `tick()` is called directly — because what matters is the
 * line, and a test that waits real seconds for a dot is a test that goes flaky exactly when it
 * is needed. The one exception is the delayed clear, which runs on its own timer and is
 * therefore given a short hold rather than the production one.
 *
 * Resource ids are faked through [format], so this stays a plain JVM test: the contract is
 * "the text comes from the resource" and the mapping itself is the resource file's job.
 */
class ConnectionProgressTest {

    /** Every value the strip was asked to show, in order. */
    private val shown = mutableListOf<String?>()

    private val current: String? get() = shown.lastOrNull()

    /** The last non-null value, for tests that render into their own list. */
    private fun current2(values: List<String?>): String? = values.lastOrNull { it != null }

    /** A formatter that renders resource ids as their string, with args appended. */
    private val format: (Int, List<Any>) -> String = { id, args ->
        when (id) {
            DNS -> "DNS lookup for home.example"
            TCP -> "TCP connection to 192.168.1.10:8443"
            TLS -> "TLS handshake with home.example"
            ATTACHED -> "Attached to ${args[0]} (${args[1]}×${args[2]})"
            CONNECTED -> "Connected"
            NO_RESPONSE -> "No response from the tab"
            else -> "phase$id"
        }
    }

    private fun progress() = ConnectionProgress(render = { shown += it }, format = format)

    // --- the line ----------------------------------------------------------

    /**
     * A phase is named, and each tick extends the same line by one full stop.
     *
     * The failure this guards is the one an append-only API would produce: a fresh call per
     * tick that replaces instead of extends, so the strip would show exactly one dot at all
     * times and never look like progress.
     */
    @Test
    fun aPhaseGainsOneDotPerTick() {
        val progress = progress()

        progress.phase(DNS)
        progress.tick()
        progress.tick()
        progress.tick()

        assertEquals("DNS lookup for home.example...", current)
    }

    /**
     * A new phase replaces the previous one instead of joining it.
     *
     * A bar has one line. Joining would give "DNS lookup for …..TCP connection to …", the
     * unreadable accumulation the strip exists to avoid.
     */
    @Test
    fun aNewPhaseReplacesThePreviousOne() {
        val progress = progress()

        progress.phase(DNS)
        progress.tick()
        progress.tick()
        progress.phase(TCP)
        progress.tick()

        assertEquals("TCP connection to 192.168.1.10:8443.", current)
    }

    /**
     * A phase with no resolvable text renders nothing at all.
     *
     * This is the transport built without a `Context` — a test, or a session torn down between
     * deciding to dial and getting there. Rendering an empty strip would put a blank bar over
     * the terminal.
     */
    @Test
    fun aPhaseWithNoTextRendersNothing() {
        val progress = ConnectionProgress(render = { shown += it }, format = { _, _ -> "" })

        progress.phase(DNS)

        assertEquals(emptyList<String?>(), shown)
    }

    // --- the detail (replay volume) ---------------------------------------

    /**
     * The replay counter changes the line without resetting its dots.
     *
     * Three parts, not one string, and this is why: the volume ticks up during a phase that is
     * still running, and losing the dots on every update would make a slow replay look like it
     * kept restarting.
     */
    @Test
    fun aPhaseKeepsItsDotsWhenTheDetailChanges() {
        val progress = progress()

        progress.phase(TLS)
        progress.tick()
        progress.tick()
        progress.detail("2.4 MB")
        progress.tick()

        assertEquals("TLS handshake with home.example 2.4 MB...", current)
    }

    /**
     * A detail that has not changed does not re-render.
     *
     * The replay counter is fed once per incoming frame and a multi-megabyte scrollback is
     * thousands of frames; each render is a `StateFlow` write and a console recomposition. The
     * failure this prevents is a UI that stutters exactly while a large tab is attaching.
     */
    @Test
    fun theDetailRendersOnlyWhenItChanges() {
        val progress = progress()

        progress.phase(TLS)
        val before = shown.size
        progress.detail("2.4 MB")
        val afterFirst = shown.size
        progress.detail("2.4 MB")
        progress.detail("2.4 MB")

        assertEquals("the first update renders", before + 1, afterFirst)
        assertEquals("repeats do not", afterFirst, shown.size)
    }

    /** A detail with no phase running has nothing to attach to. */
    @Test
    fun aDetailWithoutAPhaseRendersNothing() {
        val progress = progress()

        progress.detail("2.4 MB")

        assertEquals(emptyList<String?>(), shown)
    }

    // --- finishing, deadlines, and the success line ------------------------

    /**
     * Ticks after [ConnectionProgress.finish] change nothing that the deadline would act on.
     *
     * `finish` is called when the daemon has answered: there is no longer anything to give up
     * on, so a deadline armed before it must not fire afterwards.
     */
    @Test
    fun aDeadlineIsDisarmedByFinish() {
        val progress = progress()

        progress.phase(TLS)
        progress.deadlineAfter(0L, NO_RESPONSE)
        progress.finish()
        progress.tick()

        assertEquals("TLS handshake with home.example.", current)
    }

    /**
     * A phase that says nothing past its deadline ends visibly.
     *
     * Without this a daemon that accepts the socket and then goes quiet leaves "WebSocket open"
     * on the strip for ever, which is indistinguishable from a hang.
     */
    @Test
    fun anAttachThatSaysNothingEndsVisibly() {
        val progress = progress()

        progress.phase(TLS)
        progress.deadlineAfter(0L, NO_RESPONSE)
        progress.tick()

        assertEquals("No response from the tab", current)
    }

    /**
     * Any new phase disarms a pending deadline.
     *
     * The deadline exists to notice that *nothing happened*; a phase change is something
     * happening, so the old deadline must not fire over the new phase.
     */
    @Test
    fun aNewPhaseDisarmsAnEarlierDeadline() {
        val progress = progress()

        progress.phase(TLS)
        progress.deadlineAfter(0L, NO_RESPONSE)
        progress.phase(TCP)
        progress.tick()

        assertEquals("TCP connection to 192.168.1.10:8443.", current)
    }

    /**
     * The success line is shown, then the strip clears by itself.
     *
     * The hold is the point: clearing the instant content arrives would leave the user
     * inferring success from the absence of a bar. Given a 40 ms hold here rather than the
     * production 1.2 s so the test is quick, and awaited rather than assumed.
     */
    @Test
    fun connectedIsHeldThenCleared() {
        val progress = progress()

        progress.phase(TLS)
        progress.tick()
        progress.connectThenClear(40L, CONNECTED)

        assertEquals("Connected", current)

        val deadline = System.currentTimeMillis() + 2_000L
        while (current != null && System.currentTimeMillis() < deadline) {
            Thread.sleep(10L)
        }

        assertNull("the strip clears itself after the hold", current)
    }

    /**
     * A stopped progress ignores everything after it.
     *
     * The reason this matters is timing, not tidiness: an OkHttp callback can arrive for a
     * socket that has already been replaced, and a dial's timer can fire once more after the
     * attempt ended. Neither may repaint a strip that now belongs to a newer attempt.
     */
    @Test
    fun aStoppedProgressIgnoresEverything() {
        val progress = progress()

        progress.phase(TLS)
        progress.tick()
        progress.stop()
        val afterStop = shown.size

        progress.phase(TCP)
        progress.tick()
        progress.detail("2.4 MB")
        progress.connectThenClear(10L, CONNECTED)

        assertEquals("nothing may render after stop", afterStop, shown.size)
    }

    /**
     * Stopping clears the strip, and a second stop does not re-clear.
     *
     * The dial's cleanup runs on every path, including ones where no phase was ever named.
     */
    @Test
    fun stoppingClearsOnce() {
        val progress = progress()

        progress.phase(DNS)
        progress.tick()
        progress.stop()
        val afterFirstStop = shown.size
        progress.stop()

        assertNull(current)
        assertEquals(afterFirstStop, shown.size)
    }

    /** A tick before any phase shows nothing — no bare dot on an empty terminal. */
    @Test
    fun aTickBeforeAnyPhaseShowsNothing() {
        val progress = progress()

        progress.tick()

        assertEquals(emptyList<String?>(), shown)
    }

    // --- states versus attempts -------------------------------------------

    /**
     * A phase that describes a state earns no dots.
     *
     * `WebSocket open . . .` reads as though it were still waiting for something, and a state
     * that keeps counting cannot be told from a hang. The replay phase is the case that proves
     * the rule: its progress is the byte count, not punctuation.
     */
    @Test
    fun aSteadyPhaseDoesNotGrowDots() {
        val progress = progress()

        progress.steadyPhase(TCP)
        progress.tick()
        progress.tick()

        assertEquals("TCP connection to 192.168.1.10:8443", current)
    }

    /**
     * The replay counter is formatted and only re-rendered when it changes.
     *
     * Fed once per frame against a multi-megabyte scrollback, so an unguarded render would be
     * a console recomposition per frame. The formatter is this instance's, which is how French
     * gets its `2,4 Mo` rather than `2.4 MB`.
     */
    @Test
    fun replayBytesAreFormattedAndRenderedOnlyWhenTheyChange() {
        val rendered = mutableListOf<String?>()
        var calls = 0
        val progress = ConnectionProgress(
            render = { rendered += it },
            format = format,
            // Rounds down to whole kilobytes, so several frames map to one text.
            formatBytes = { bytes -> calls++; "${bytes / 1_000L} kB" },
        )

        progress.steadyPhase(TLS)
        val before = rendered.size
        progress.addReplayBytes(100L)
        progress.addReplayBytes(100L)
        progress.addReplayBytes(1_000L)

        assertEquals("TLS handshake with home.example 1 kB", current2(rendered))
        assertEquals("three updates, two renders", before + 2, rendered.size)
        assertEquals("formatted per update", 3, calls)
    }

    /**
     * Once the connect is finished, a frame still arriving may not change the line.
     *
     * This is the bug the design review caught: the daemon keeps replaying while the success
     * line is held, and an unguarded detail update renders `Connected 2.4 MB`.
     */
    @Test
    fun replayStopsCountingOnceConnected() {
        val progress = progress()

        progress.steadyPhase(TLS)
        progress.addReplayBytes(1_000L)
        progress.connectThenClear(40L, CONNECTED)
        progress.addReplayBytes(5_000_000L)
        progress.phase(TCP)

        assertEquals("Connected", current)
    }

    /**
     * The success line waits until the phase it replaces has been readable.
     *
     * `connect()` returns at the upgrade and the Relay only starts afterwards, so the daemon
     * can have queued the entire replay in between: without the dwell the counter flashes by
     * faster than it can be read and the phase may as well not exist.
     */
    @Test
    fun connectedWaitsForTheReplayToBeReadable() {
        val progress = progress()

        progress.steadyPhase(TLS)
        progress.connectThenClear(40L, CONNECTED, minPhaseVisibleMillis = 200L)

        // Still describing the phase just entered — the dwell has not elapsed.
        assertEquals("the replay stays on screen", "TLS handshake with home.example", current)

        val deadline = System.currentTimeMillis() + 2_000L
        while (current != "Connected" && System.currentTimeMillis() < deadline) {
            Thread.sleep(10L)
        }
        assertEquals("then the success line", "Connected", current)
    }

    // --- the byte count ----------------------------------------------------

    /**
     * The replay volume reads the way a person says it.
     *
     * The boundary that matters is a megabyte: below it the number is in kilobytes and a whole
     * one is fine, above it a decimal is what makes the figure visibly move on a long replay.
     */
    @Test
    fun replayVolumeIsFormattedForReading() {
        assertEquals("0 B", ConnectionProgress.formatBytes(0))
        assertEquals("512 B", ConnectionProgress.formatBytes(512))
        assertEquals("1 kB", ConnectionProgress.formatBytes(1_000))
        assertEquals("999 kB", ConnectionProgress.formatBytes(999_999))
        assertEquals("1.0 MB", ConnectionProgress.formatBytes(1_000_000))
        assertEquals("2.4 MB", ConnectionProgress.formatBytes(2_450_000))
    }

    /**
     * A negative count is treated as nothing rather than rendered with a minus sign.
     *
     * The counter is only added to, so this cannot happen today — but a figure that can only
     * be produced by a bug should read as zero, not as "-1 B" on the user's screen.
     */
    @Test
    fun aNegativeVolumeReadsAsZero() {
        assertEquals("0 B", ConnectionProgress.formatBytes(-1))
    }

    private companion object {
        const val DNS = 1
        const val TCP = 2
        const val TLS = 3
        const val ATTACHED = 4
        const val CONNECTED = 5
        const val NO_RESPONSE = 6
    }
}
