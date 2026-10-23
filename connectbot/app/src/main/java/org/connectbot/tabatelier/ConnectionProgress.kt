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

/**
 * Builds the one-line description of what a connection is doing:
 *
 * ```
 * DNS lookup for home.example = 192.168.1.10
 * TCP connection to 192.168.1.10:8443 ..
 * TLS handshake with home.example (TLS_1_3)
 * Opening WebSocket…
 * WebSocket open
 * Attached to fix-ci (193×54)
 * Replaying scrollback… 2.4 MB
 * Connected
 * ```
 *
 * A phase is named as it starts and — while it is *work in progress* — gains one full stop per
 * second, so a connection that is slow is visibly slow rather than indistinguishable from one
 * that has hung. Phases that describe a *state* rather than an attempt ([steadyPhase]) do not
 * tick: `WebSocket open . . .` would suggest it was still waiting for something.
 *
 * ## Three parts, not one string
 *
 * The line is [base] + optional [detail] + the dots. Keeping them apart is what lets the
 * replay counter update mid-phase (`Replaying scrollback… 2.4 MB`) without resetting the dots —
 * and, because the detail only re-renders when its text changes, without waking the UI once per
 * incoming frame. A multi-megabyte replay arrives as thousands of frames; a render per frame
 * would be a recomposition per frame.
 *
 * ## Why nothing is written into the terminal
 *
 * The terminal is the remote program's surface, and text the app invents lands among the
 * program's output — where it can be mistaken for something the server said, and where it has
 * to be erased again afterwards, which is a second way to disturb bytes the user is reading.
 * A connection is *this* end's business, so it is described on this end's chrome.
 *
 * ## Lifecycle
 *
 * One instance per dial attempt: created by `TabAtelier.openSocketAt`, started, then either
 * stopped (the attempt failed) or handed to the attach phase (the socket opened). It therefore
 * outlives the dialling function on success — see `activeProgress` there.
 *
 * [stop] is final and so is [connectThenClear]: a late OkHttp callback, a timer already in
 * flight, or a replay frame still arriving must not repaint a strip that has moved on. The
 * second matters as much as the first — without it, a frame arriving during the closing hold
 * would render `Connected 2.4 MB`.
 */
class ConnectionProgress(
    /** Receives the full current line, or null when there is nothing to say. */
    private val render: (String?) -> Unit,
    /**
     * Resolves a string resource with positional arguments.
     *
     * Injected rather than taking a `Context`, because everything here is pure formatting and
     * the tests are plain JVM ones. The transport supplies
     * `manager.getString(id, *args.toTypedArray())`; a test supplies a map.
     */
    private val format: (Int, List<Any>) -> String,
    /**
     * A byte count as a person reads it.
     *
     * Injected because the units are part of the sentence: English writes `2.4 MB`, French
     * `2,4 Mo`. Production passes Android's locale-aware formatter; the default below is the
     * English/Latin fallback for tests.
     */
    private val formatBytes: (Long) -> String = ::formatBytes,
    private val tickMillis: Long = DEFAULT_TICK_MILLIS,
) {
    private val lock = Any()
    private var timer: java.util.Timer? = null

    /** The one-shot that clears the strip after [connectThenClear]'s hold. */
    private var holdTimer: java.util.Timer? = null

    /** The phase text, without detail or dots. Null when nothing is running. */
    private var base: String? = null

    /** A changing suffix inside a phase — the replay volume. */
    private var detail: String? = null

    /** Full stops earned so far, one per tick. */
    private var dots = 0

    /** Whether the running phase keeps earning dots. See the class comment. */
    private var ticking = true

    /** Whether anything has been shown since the last [stop], for [stop]'s sake. */
    private var hadSomething = false

    /** When the running phase should give up, or 0 for no deadline. */
    private var deadlineAt = 0L

    /** What to say when that deadline passes. */
    private var deadlineText: String? = null

    /** When the current phase first appeared, for [connectThenClear]'s minimum dwell. */
    private var phaseShownAt = 0L

    /** Bytes replayed in the running phase, formatted through [formatBytes]. */
    private var replayBytes = 0L

    /** Set by [stop]; a stopped instance does nothing at all. See the class comment. */
    private var stopped = false

    /** Set by [connectThenClear]; a finished connect accepts no further phases. */
    private var finished = false

    /**
     * Starts ticking.
     *
     * Separate from construction so the caller decides when the thread exists: a transport is
     * built before anyone connects, and a timer started there would tick through an idle app.
     */
    fun start() = synchronized(lock) {
        if (timer != null || stopped) return@synchronized
        val t = java.util.Timer("tabatelier-connect-progress", true)
        t.scheduleAtFixedRate(
            object : java.util.TimerTask() {
                override fun run() = tick()
            },
            tickMillis,
            tickMillis,
        )
        timer = t
    }

    /**
     * Stops ticking and clears the line. Safe to call more than once, and final.
     *
     * Final matters: an OkHttp callback can arrive after the socket it belongs to has been
     * replaced, and a dial's timer can fire once more after the phase ended. Neither may
     * repaint a strip that now belongs to a newer attempt.
     */
    fun stop() = synchronized(lock) {
        if (stopped) return@synchronized
        stopped = true
        cancelTimers()
        base = null
        detail = null
        dots = 0
        deadlineAt = 0
        deadlineText = null
        // Only if something was actually shown. The dial stops on every path, including ones
        // where no phase was ever named, and clearing a strip that is already empty is a
        // repaint of nothing — which the console would still be asked to recompose for.
        if (hadSomething) render(null)
        hadSomething = false
    }

    /**
     * Ends the phase in progress, if any, without clearing what has been drawn.
     *
     * Kept for callers that only want to call off the deadline — [steadyPhase] is what ends a
     * phase's dots.
     */
    fun finish() = synchronized(lock) {
        if (stopped) return@synchronized
        deadlineAt = 0
        deadlineText = null
    }

    /**
     * Starts a phase that describes an attempt in progress, replacing the line.
     *
     * No accumulation: a bar has one line, and the previous phase's text is simply gone. That
     * is what makes this safe to drive from callbacks that can fire in any order — the last
     * event wins instead of every event appending.
     *
     * A phase with no text (no resources to resolve one against) is a no-op rather than an
     * empty strip, so a transport built without a `Context` renders nothing at all.
     */
    fun phase(resId: Int, vararg args: Any) = show(resId, args, ticking = true)

    /**
     * Starts a phase that describes a *state* — the socket is open, the tab is attached, the
     * replay is running — rather than an attempt at something.
     *
     * Such a phase earns no dots: `WebSocket open . . .` reads as though it were still waiting,
     * and a state that keeps counting is a state the reader cannot tell from a hang. The replay
     * phase is the case that proves the rule: its progress is the byte count, not punctuation.
     */
    fun steadyPhase(resId: Int, vararg args: Any) = show(resId, args, ticking = false)

    /** [phase] for text already resolved — the fallback host name. */
    fun phaseText(text: String) = synchronized(lock) {
        if (stopped || finished || text.isEmpty()) return@synchronized
        apply(text, ticking = true)
    }

    /**
     * Sets or clears the suffix inside the running phase — the replay volume.
     *
     * Re-renders only when the text actually changed. The replay counter is fed once per
     * incoming frame, and a multi-megabyte scrollback is thousands of frames: an unguarded
     * render would mean a `StateFlow` write and a console recomposition for each one.
     */
    fun detail(text: String?) = synchronized(lock) {
        if (stopped || finished || base == null) return@synchronized
        if (detail == text) return@synchronized
        detail = text
        renderLine()
    }

    /**
     * Adds to the replay total and shows it, without the caller counting.
     *
     * The count lives here rather than in the transport so that it is formatted through this
     * instance's [formatBytes] — the units are part of the sentence and belong with the words.
     */
    fun addReplayBytes(count: Long) = synchronized(lock) {
        if (stopped || finished || base == null) return@synchronized
        replayBytes += count
        val text = formatBytes(replayBytes)
        if (detail == text) return@synchronized
        detail = text
        renderLine()
    }

    /**
     * Gives the running phase an ending, for the case where nothing else ever happens.
     *
     * A daemon that accepts the socket and then says nothing would otherwise leave a bar
     * saying "WebSocket open" forever, which looks like a hang and is indistinguishable from
     * one. On a tick past the deadline the line is replaced by [resId] and the strip clears
     * after a hold — long enough to read, short enough not to linger.
     */
    fun deadlineAfter(millis: Long, resId: Int) = synchronized(lock) {
        if (stopped || finished) return@synchronized
        deadlineAt = System.currentTimeMillis() + millis
        deadlineText = format(resId, emptyList())
    }

    /**
     * Ends a successful connect: show the final line, then clear after [holdMillis].
     *
     * The hold is what makes success legible. Clearing the moment content arrives would leave
     * the user inferring that the connection worked from the absence of a bar; a "Connected"
     * that lingers a moment says so, and then gets out of the way.
     *
     * [minPhaseVisibleMillis] keeps the phase it replaces on screen long enough to read. The
     * need is specific to this protocol: `connect()` returns at the upgrade, the Relay only
     * starts after that, and the daemon may have queued the whole scrollback in between — so
     * without a dwell the replay counter can go from appearing to "Connected" faster than
     * anyone can read the number, and the phase may as well not exist.
     *
     * After this, the instance is finished: no frame, tick or callback may change the line.
     */
    fun connectThenClear(
        holdMillis: Long,
        resId: Int,
        minPhaseVisibleMillis: Long = 0L,
    ) = synchronized(lock) {
        if (stopped || finished) return@synchronized
        finished = true
        deadlineAt = 0
        deadlineText = null
        val text = format(resId, emptyList())
        if (text.isEmpty()) {
            stop()
            return@synchronized
        }
        // The phase it replaces has to have been readable; anything already older than the
        // minimum is replaced at once.
        val shownFor = System.currentTimeMillis() - phaseShownAt
        val wait = (minPhaseVisibleMillis - shownFor).coerceAtLeast(0L)
        if (wait == 0L) {
            showConnected(text, holdMillis)
        } else {
            cancelTimers()
            scheduleOnce(wait) {
                if (!stopped) showConnected(text, holdMillis)
            }
        }
    }

    /** One tick: a full stop for a ticking phase, or the deadline giving up. */
    fun tick() = synchronized(lock) {
        if (stopped || finished || base == null) return@synchronized

        if (deadlineAt > 0 && System.currentTimeMillis() >= deadlineAt) {
            val text = deadlineText
            deadlineAt = 0
            deadlineText = null
            if (!text.isNullOrEmpty()) {
                finished = true
                base = text
                detail = null
                dots = 0
                renderLine()
                cancelTimers()
                scheduleOnce(HOLD_MILLIS) { stop() }
            }
            return@synchronized
        }

        if (!ticking) return@synchronized
        dots++
        renderLine()
    }

    /** The current line, for callers and tests that want to read it back. */
    fun currentLine(): String? = synchronized(lock) { lineOf() }

    /** Renders [text] as the final line and schedules the clear. Caller holds [lock]. */
    private fun showConnected(text: String, holdMillis: Long) {
        base = text
        detail = null
        dots = 0
        phaseShownAt = System.currentTimeMillis()
        renderLine()
        // The connect has an end, so nothing is left to count down.
        cancelTimers()
        scheduleOnce(holdMillis) { stop() }
    }

    /** Applies a phase from [phase]/[steadyPhase]. Caller holds [lock]. */
    private fun show(resId: Int, args: Array<out Any>, ticking: Boolean) = synchronized(lock) {
        if (stopped || finished) return@synchronized
        val text = format(resId, args.toList())
        if (text.isEmpty()) return@synchronized
        apply(text, ticking)
    }

    /** Sets the phase text and re-renders. Caller holds [lock]. */
    private fun apply(text: String, ticking: Boolean) {
        base = text
        detail = null
        dots = 0
        replayBytes = 0L
        this.ticking = ticking
        phaseShownAt = System.currentTimeMillis()
        // Any phase change disarms a deadline armed by an earlier one: the thing the deadline
        // was waiting for has just happened.
        deadlineAt = 0
        deadlineText = null
        hadSomething = true
        renderLine()
    }

    /** Builds and renders the line from its parts. Caller holds [lock]. */
    private fun renderLine() {
        val text = lineOf() ?: return
        render(text)
    }

    /** [base] + optional detail + dots, or null when there is no phase. */
    private fun lineOf(): String? {
        val b = base ?: return null
        val d = detail
        val withDetail = if (d.isNullOrEmpty()) b else "$b $d"
        return withDetail + DOT.repeat(dots)
    }

    /** Cancels the ticker and any pending one-shot. Caller holds [lock]. */
    private fun cancelTimers() {
        timer?.cancel()
        timer = null
        holdTimer?.cancel()
        holdTimer = null
    }

    /** Runs [action] once, [millis] from now, on a timer of its own. Caller holds [lock]. */
    private fun scheduleOnce(millis: Long, action: () -> Unit) {
        holdTimer?.cancel()
        val t = java.util.Timer("tabatelier-connect-progress-clear", true)
        t.schedule(
            object : java.util.TimerTask() {
                override fun run() = action()
            },
            millis,
        )
        holdTimer = t
    }

    companion object {
        private const val DOT = "."

        /** One per second, which is what was asked for and what a reader can count. */
        const val DEFAULT_TICK_MILLIS = 1000L

        /** How long a final line — "Connected", or a timeout — stays before clearing. */
        const val HOLD_MILLIS = 1_200L

        /**
         * How long the replay phase is kept readable before it is replaced by "Connected".
         *
         * A replay that is already queued in the socket's buffer reaches the transport faster
         * than a reader can focus; the number would appear and vanish.
         */
        const val REPLAY_MIN_VISIBLE_MILLIS = 600L

        /**
         * A byte count as a person reads it: `512 B`, `999 kB`, `2.4 MB`.
         *
         * Decimal units, and the megabyte boundary is at a round million rather than at 1024²:
         * with binary units a figure just under a megabyte reads `1047 kB`, four digits of
         * kilobytes at the exact point a reader is looking for the scale to change. Decimal
         * keeps every unit inside three digits.
         *
         * The default for [formatBytes]; production replaces it with Android's locale-aware
         * formatter, because French writes `2,4 Mo`.
         */
        fun formatBytes(bytes: Long): String = when {
            bytes <= 0L -> "0 B"
            bytes < 1_000L -> "$bytes B"
            bytes < 1_000_000L -> "${bytes / 1_000L} kB"
            else -> {
                val tenths = bytes * 10 / 1_000_000L
                "${tenths / 10}.${tenths % 10} MB"
            }
        }
    }
}
