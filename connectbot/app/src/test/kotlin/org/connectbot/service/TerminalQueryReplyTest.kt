/*
 * ConnectBot: simple, powerful, open-source SSH client for Android
 * Copyright 2026 Kenny Root
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

/**
 * New file for Tab Atelier Remote (Apache-2.0 section 4(b)), ours — upstream has no
 * reason to filter this, because upstream never replays a scrollback into a fresh
 * terminal.
 *
 * A tab-atelier session replays the tab's whole scrollback when a viewer attaches, and
 * that replay contains the device queries other programs printed. The emulator answers
 * them again, every attach, and those answers travel back as session input — so a bash
 * prompt grew `|libvterm(0.3)\x1b\\\x1b[?1;2c`, which is this terminal's XTVERSION reply
 * followed by its DA1 reply, typed into the shell.
 *
 * The first test is the reported string, byte for byte, because that is the thing that
 * was actually seen on a prompt. The rest are the boundaries: what else the terminal
 * answers, and the one answer that must survive.
 */
package org.connectbot.service

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class TerminalQueryReplyTest {

    private val esc = 0x1b.toByte()

    private fun bytes(s: String): ByteArray = s.toByteArray(Charsets.UTF_8)

    /**
     * The two replies that were reported, in the shape the symptom showed: the version
     * answer, then device attributes, arriving as input.
     */
    @Test
    fun theReportedInjectionIsRecognised() {
        // ESC P > | l i b v t e r m ( 0 . 3 ) ESC \
        val xtversion = byteArrayOf(esc) + "P>|libvterm(0.3)".toByteArray() + byteArrayOf(esc, 0x5c)
        assertTrue(isTerminalQueryReply(xtversion))

        // ESC [ ? 1 ; 2 c
        assertTrue(isTerminalQueryReply(bytes("\u001b[?1;2c")))
    }

    @Test
    fun deviceAttributeRepliesAreRecognised() {
        assertTrue(isTerminalQueryReply(bytes("\u001b[?6c")))
        assertTrue(isTerminalQueryReply(bytes("\u001b[>0;95;0c")))
        assertTrue(isTerminalQueryReply(bytes("\u001b[?1;2;3;4c")))
    }

    /**
     * A device-control string of any content, because that is the framing XTVERSION uses
     * and a different terminal would answer with a different name.
     */
    @Test
    fun deviceControlStringsAreRecognised() {
        assertTrue(isTerminalQueryReply(byteArrayOf(esc) + "P>|other-term(9.9)".toByteArray() + byteArrayOf(esc, 0x5c)))
        assertTrue(isTerminalQueryReply(byteArrayOf(esc) + "P1\$r1234abcd".toByteArray() + byteArrayOf(esc, 0x5c)))
    }

    /**
     * A cursor-position report must survive, and it is the one answer that must: a
     * program asks where the cursor is when it is waiting for that answer, unlike the
     * two above, which the replay re-asks into the void. Dropping this would break the
     * programs that use it rather than the replay that does not.
     */
    @Test
    fun aCursorPositionReportIsLeftAlone() {
        assertFalse(isTerminalQueryReply(bytes("\u001b[12;40R")))
        assertFalse(isTerminalQueryReply(bytes("\u001b[1;1R")))
    }

    @Test
    fun typedInputIsLeftAlone() {
        assertFalse(isTerminalQueryReply(bytes("ls -la\n")))
        assertFalse(isTerminalQueryReply(bytes("\t")))
        assertFalse(isTerminalQueryReply(bytes("c")))
        assertFalse(isTerminalQueryReply(ByteArray(0)))
        // An escape the user sent deliberately, as a key or a paste.
        assertFalse(isTerminalQueryReply(bytes("\u001b")))
        assertFalse(isTerminalQueryReply(bytes("\u001b[A"))) // up arrow
    }

    /**
     * Anything that merely starts like a reply is not one.
     *
     * Each reply arrives as its own callback, so a payload that begins with one and
     * carries on is something else — and matching it would swallow real input rather
     * than stop a spurious answer.
     */
    @Test
    fun aPayloadThatOnlyStartsLikeAReplyIsNotOne() {
        assertFalse(isTerminalQueryReply(bytes("\u001b[?1;2cand then some input")))
        assertFalse(isTerminalQueryReply(byteArrayOf(esc) + "P>|libvterm(0.3)".toByteArray()))
    }
}
