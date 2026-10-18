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

import java.util.Timer
import java.util.TimerTask

/**
 * Writes what a connection is doing, phase by phase, into the terminal:
 *
 * ```
 * DNS lookup for home.example
 * TCP connection to 1.2.3.4:8443 .
 * TLS handshake with home.example ...
 * ```
 *
 * A new line starts for each phase and gains one full stop per second it runs, so a
 * connection that is slow is visibly slow rather than indistinguishable from one that has
 * hung. When a phase ends its line is terminated and the next one begins.
 *
 * ## Why a ticker rather than a status line that gets redrawn
 *
 * The destination is a terminal emulator, not a text view, and the emulator belongs to the
 * terminal library: there is no spinner to update and no single status field to overwrite.
 * Writing dots is what a terminal can do cheaply and what a person reading one expects — it
 * is also what `curl -v` and `ssh -v` look like, which is the vocabulary this borrows.
 *
 * ## Why one dot per second
 *
 * DNS is usually over in tens of milliseconds, so most lines read `DNS lookup for …` and
 * nothing else: the *line* is the information, the dots are for the case that needs them. A
 * faster tick would smear every line with punctuation and wake a thread far more often for
 * it.
 *
 * [start] must be called or nothing ticks, and [stop] must be called or the thread outlives
 * the connection. Both are done by the transport, which owns one of these per dial.
 */
class ConnectionProgress(
    /** Appends text at the cursor. No newline is implied. */
    private val write: (String) -> Unit,
    private val tickMillis: Long = DEFAULT_TICK_MILLIS,
) {
    private val lock = Any()
    private var timer: Timer? = null

    /** Whether a phase line is open and gaining dots. */
    private var running = false

    /**
     * Starts ticking.
     *
     * Separate from construction so that the caller decides when the thread exists: a
     * `TabAtelier` transport is built before anyone connects, and a timer started there
     * would tick through an idle app.
     */
    fun start() = synchronized(lock) {
        if (timer != null) return@synchronized
        val t = Timer("tabatelier-connect-progress", true)
        t.scheduleAtFixedRate(
            object : TimerTask() {
                override fun run() = tick()
            },
            tickMillis,
            tickMillis,
        )
        timer = t
    }

    /** Stops ticking. Safe to call more than once. */
    fun stop() = synchronized(lock) {
        timer?.cancel()
        timer = null
    }

    /**
     * Ends the phase in progress, if any, and starts [text].
     *
     * Either the previous phase was the one that succeeded — in which case the connection is
     * established and this is the last phase — or it failed, and the line it left behind
     * should stand as it was rather than be rewritten after the fact.
     */
    fun phase(text: String) = synchronized(lock) {
        if (running) write(NEWLINE)
        running = true
        write(text)
    }

    /**
     * Ends the phase in progress.
     *
     * The line is terminated but not otherwise touched: its dots are the record of how long
     * it took. Rewriting it to say "done in 0.2s" would mean emitting a carriage return into
     * a terminal whose scrolled-back output is what the user reads afterwards.
     */
    fun finish() = synchronized(lock) {
        if (!running) return@synchronized
        running = false
        write(NEWLINE)
    }

    /** One tick: a dot for the running phase, if there is one. */
    fun tick() = synchronized(lock) {
        if (!running) return@synchronized
        write(DOT)
    }

    companion object {
        private const val NEWLINE = "\r\n"
        private const val DOT = "."

        /** One per second, which is what was asked for and what a reader can count. */
        const val DEFAULT_TICK_MILLIS = 1000L
    }
}
