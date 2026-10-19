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
 * TLS handshake with home.example ...
 * ```
 *
 * Each phase is named as it starts and gains one full stop per second it runs, so a
 * connection that is slow is visibly slow rather than indistinguishable from one that has
 * hung. [render] is handed the whole line every time it changes, rather than being asked to
 * append: the destination is a status bar that replaces its text, not an output stream, and
 * a bar that could only append would need a way to be cleared.
 *
 * ## Why nothing is written into the terminal
 *
 * This used to print these lines into the terminal emulator. It does not, and must not: the
 * terminal is the remote program's surface, and text the app invents lands among the
 * program's own output — where it can be mistaken for something the server said, and where
 * it has to be erased again afterwards, which is a second way to disturb bytes the user is
 * reading. A connection is *this* end's business, so it is described on this end's chrome.
 */
class ConnectionProgress(
    /** Receives the full current line, or null when there is nothing to say. */
    private val render: (String?) -> Unit,
    private val tickMillis: Long = DEFAULT_TICK_MILLIS,
) {
    private val lock = Any()
    private var timer: java.util.Timer? = null

    /** The phase being timed, or null when nothing is running. */
    private var phase: String? = null

    /** The line currently on screen, so a tick can extend rather than rebuild it. */
    private var line: String = ""

    /** Whether anything has been shown since the last [stop], for [stop]'s sake. */
    private var hadSomething = false

    /**
     * Starts ticking.
     *
     * Separate from construction so the caller decides when the thread exists: a transport is
     * built before anyone connects, and a timer started there would tick through an idle app.
     */
    fun start() = synchronized(lock) {
        if (timer != null) return@synchronized
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

    /** Stops ticking and clears the line. Safe to call more than once. */
    fun stop() = synchronized(lock) {
        timer?.cancel()
        timer = null
        phase = null
        line = ""
        // Only if something was actually shown. The dial stops on every path, including ones
        // where no phase was ever named, and clearing a strip that is already empty is a
        // repaint of nothing — which the console would still be asked to recompose for.
        //
        // `line` cannot answer this: it is emptied above, so the flag is what remembers.
        if (hadSomething) render(null)
        hadSomething = false
    }

    /** Ends the phase in progress, if any, without clearing what has been drawn. */
    fun finish() = synchronized(lock) {
        phase = null
    }

    /**
     * Starts a phase, replacing the line.
     *
     * No newline, no accumulation: a bar has one line, and the previous phase's text is
     * simply gone. That is what makes this safe to drive from callbacks that can fire in any
     * order — the last event wins instead of every event appending.
     */
    fun phase(text: String) = synchronized(lock) {
        phase = text
        line = text
        hadSomething = true
        render(line)
    }

    /** One tick: a full stop for the running phase, if there is one. */
    fun tick() = synchronized(lock) {
        if (phase == null) return@synchronized
        line += DOT
        render(line)
    }

    companion object {
        private const val DOT = "."

        /** One per second, which is what was asked for and what a reader can count. */
        const val DEFAULT_TICK_MILLIS = 1000L
    }
}
