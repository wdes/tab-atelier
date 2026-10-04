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
 * New file for Tab Atelier Remote (Apache-2.0 section 4(b)), ours — upstream has
 * no per-tab background, so there is nothing to preserve here.
 *
 * The daemon reports each tab's viewer background so two tabs can be told apart,
 * and it arrives as a `#rrggbb` string. Turning that into a colour is where a
 * malformed value would throw, so the parsing is a function of its own and tested
 * directly — a crash on opening a tab, or a tab rendered in an unreadable colour,
 * is far worse than a tab painted in the default.
 *
 * Robolectric is needed for `android.graphics.Color`, which `toColorInt` parses
 * through.
 */
package org.connectbot.ui.screens.console

import androidx.compose.ui.graphics.Color
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner

@RunWith(RobolectricTestRunner::class)
class TerminalColorTest {

    @Test
    fun aHexColourIsReadAsItself() {
        // The two the live daemon actually sends, checked against their values
        // rather than merely "not null": the point of the background is that a
        // user can tell two tabs apart, which a wrong-but-valid colour would
        // defeat as surely as a missing one.
        assertEquals(Color(0xFF002451), terminalColorOrNull("#002451"))
        assertEquals(Color(0xFF451C2E), terminalColorOrNull("#451c2e"))
        assertEquals(Color(0xFFFFFFFF), terminalColorOrNull("#FFFFFF"))
        assertEquals(Color(0xFF000000), terminalColorOrNull("#000000"))
    }

    @Test
    fun aStringThatIsNotAColourIsRejected() {
        // The transport checks the shape before passing one on, so these are the
        // second line of defence: what arrives here is not assumed to be
        // well-formed just because something else was meant to check it.
        //
        // "red" is why the shape is checked here at all. Android's own parser
        // accepts CSS colour names, so it would return a perfectly good red for it
        // — and a tab would then be coloured by Android's name table rather than by
        // what the daemon sent. Nothing sends one; the point is that the two checks
        // must not disagree about what is valid.
        for (bad in listOf("", "red", "transparent", "#GGGGGG", "#12345", "#1234567", "002451", "rgb(0,0,0)", "#")) {
            assertNull("\"$bad\" is not a colour and must be refused", terminalColorOrNull(bad))
        }
    }

    /**
     * The shape is `#rrggbb`, exactly. `#aarrggbb` is a colour Android would take,
     * but not one this daemon sends, and accepting a format the sender never uses
     * is how a rendering path quietly stops matching its contract.
     */
    @Test
    fun aLongerFormIsNotTheDaemonsFormat() {
        assertNull(terminalColorOrNull("#00002451"))
        assertNull(terminalColorOrNull("#FFF"))
        assertNull(terminalColorOrNull("#FFFFFFF"))
    }
}
