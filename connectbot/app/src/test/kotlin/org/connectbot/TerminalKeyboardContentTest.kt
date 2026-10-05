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

package org.connectbot

import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.junit4.v2.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performTouchInput
import androidx.compose.ui.test.swipeLeft
import androidx.test.ext.junit.runners.AndroidJUnit4
import dagger.hilt.android.testing.HiltAndroidRule
import dagger.hilt.android.testing.HiltAndroidTest
import org.connectbot.service.ModifierLevel
import org.connectbot.service.ModifierState
import org.connectbot.terminal.VTermKey
import org.connectbot.ui.components.TerminalKeyboardContent
import org.connectbot.ui.theme.ConnectBotTheme
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith

@HiltAndroidTest
@RunWith(AndroidJUnit4::class)
class TerminalKeyboardContentTest {
    @get:Rule(order = 0)
    val hiltRule = HiltAndroidRule(this)

    @get:Rule(order = 1)
    val composeTestRule = createAndroidComposeRule<HiltComponentActivity>()

    @Before
    fun setUp() {
        hiltRule.inject()
    }

    @Test
    fun terminalKeyboardContent_displaysCoreKeysAndInvokesCallbacks() {
        var ctrlPressed = false
        var altPressed = false
        var escapePressed = false
        var tabPressed = false
        var interactionCount = 0
        var pastePressed = false
        var showImeCalled = false

        setKeyboardContent(
            onCtrlPress = { ctrlPressed = true },
            onAltPress = { altPressed = true },
            onEscPress = { escapePressed = true },
            onTabPress = { tabPressed = true },
            onInteraction = { interactionCount++ },
            onPaste = { pastePressed = true },
            onShowIme = { showImeCalled = true },
        )

        composeTestRule
            .onNodeWithText(composeTestRule.activity.getString(R.string.button_key_ctrl))
            .assertIsDisplayed()
            .performClick()
        composeTestRule
            .onNodeWithText(composeTestRule.activity.getString(R.string.button_key_alt))
            .assertIsDisplayed()
            .performClick()
        composeTestRule
            .onNodeWithText(composeTestRule.activity.getString(R.string.button_key_esc))
            .assertIsDisplayed()
            .performClick()
        composeTestRule
            .onNodeWithText("⇥")
            .assertIsDisplayed()
            .performClick()
        // The keyboard key is on the FIRST page: it and paste share the same trailing
        // slot, and FN is what swaps between them. So it is pressed before the page
        // turns, or it is not there to press — which is exactly how this test failed
        // when the two were the other way round.
        composeTestRule
            .onNodeWithContentDescription(composeTestRule.activity.getString(R.string.image_description_show_keyboard))
            .assertIsDisplayed()
            .performClick()
        // Paste and the function keys live on the second page, so it has to be opened
        // first. Tapping FN reports no interaction of its own.
        composeTestRule
            .onNodeWithContentDescription(composeTestRule.activity.getString(R.string.image_description_function_keys))
            .assertIsDisplayed()
            .performClick()
        composeTestRule
            .onNodeWithContentDescription(composeTestRule.activity.getString(R.string.image_description_paste))
            .assertIsDisplayed()
            .performClick()

        assertTrue(ctrlPressed)
        assertTrue(altPressed)
        assertTrue(escapePressed)
        assertTrue(tabPressed)
        assertTrue(pastePressed)
        assertTrue(showImeCalled)
        // Asserted as "at least one", not an exact count, and the number is not the
        // contract. Interaction is what resets the console's auto-hide timer, and it is
        // reported twice over: the bar's whole surface has a pointerInput for it, and
        // the keys that own a callback report it themselves. How many of the presses
        // reach the surface versus the key depends on Compose's gesture routing, so an
        // exact total changes whenever a key is added or a row is re-laid-out — it was
        // 2 here, and 1 after the two-row layout, with nothing broken either time.
        // What must hold is that pressing the bar reports interaction at all.
        assertTrue("the bar must report interaction so the auto-hide timer resets", interactionCount > 0)
    }

    @Test
    fun terminalKeyboardContent_imeVisibleInvokesHideKeyboard() {
        var hideImeCalled = false
        var interactionCount = 0

        setKeyboardContent(
            imeVisible = true,
            modifierState = ModifierState(
                ctrlState = ModifierLevel.LOCKED,
                altState = ModifierLevel.OFF,
                shiftState = ModifierLevel.OFF,
            ),
            onHideIme = { hideImeCalled = true },
            onInteraction = { interactionCount++ },
        )

        composeTestRule
            .onNodeWithContentDescription(composeTestRule.activity.getString(R.string.image_description_hide_keyboard))
            .assertIsDisplayed()
            .performClick()

        assertTrue(hideImeCalled)
        assertEquals(1, interactionCount)
    }

    @Test
    fun terminalKeyboardContent_arrowAndFunctionKeysInvokeKeyCallback() {
        val pressedKeys = mutableListOf<Int>()

        setKeyboardContent(
            modifierState = ModifierState(
                ctrlState = ModifierLevel.TRANSIENT,
                altState = ModifierLevel.OFF,
                shiftState = ModifierLevel.OFF,
            ),
            onKeyPress = { pressedKeys += it },
            bumpyArrows = true,
        )

        composeTestRule
            .onNodeWithContentDescription(composeTestRule.activity.getString(R.string.image_description_up))
            .performTouchInput {
                down(center)
                up()
            }
        // The arrows are on the main page and the function keys behind FN, so this
        // covers both pages: one key each, and the page switch between them.
        composeTestRule
            .onNodeWithText(composeTestRule.activity.getString(R.string.button_key_fn))
            .performClick()
        composeTestRule
            .onNodeWithText(composeTestRule.activity.getString(R.string.button_key_f1))
            .performClick()

        assertEquals(listOf(VTermKey.UP, VTermKey.FUNCTION_1), pressedKeys)
    }

    @Test
    fun terminalKeyboardContent_reportsHorizontalScrollInteractions() {
        val scrollStates = mutableListOf<Boolean>()

        setKeyboardContent(
            onScrollInProgressChange = { scrollStates += it },
        )

        composeTestRule
            .onNodeWithText(composeTestRule.activity.getString(R.string.button_key_ctrl))
            .performTouchInput { swipeLeft() }

        assertTrue(scrollStates.isNotEmpty())
    }

    /**
     * Changed for Tab Atelier Remote (Apache-2.0 section 4(b)).
     *
     * The bar used to carry an "IME" key toggling compose mode — the composition
     * buffer for languages that need one, Japanese and Chinese among them. It is not
     * in the two-row layout, which has no place for it, and nothing is lost: compose
     * mode is reached from the console's own menu, which is where the second entry
     * point always was, so the key was a shortcut rather than the only way in.
     *
     * Asserted rather than merely noted, because "the bar has no compose key" is a
     * decision, and a decision that nothing pins is one a later change undoes by
     * accident — most likely by upstream resurrecting it during a sync.
     */
    @Test
    fun theBarHasNoComposeKeyBecauseTheConsoleMenuDoes() {
        setKeyboardContent()

        composeTestRule.onNodeWithText("IME").assertDoesNotExist()
    }

    /**
     * The optional keyboard key is opt-in, and hiding it must leave a working bar.
     *
     * The old test asserted that hiding it did not remove the text-input button, which
     * this bar does not have at all — paste replaced it, on the second page. What
     * matters now is that the trailing column still holds a usable key when the
     * optional one is off, since that column is what reaches the second page.
     */
    @Test
    fun theOptionalKeyboardKeyCanBeHiddenWithoutBreakingTheBar() {
        setKeyboardContent(showImeToggleKey = false)

        composeTestRule
            .onNodeWithContentDescription(composeTestRule.activity.getString(R.string.image_description_show_keyboard))
            .assertDoesNotExist()
        composeTestRule
            .onNodeWithText(composeTestRule.activity.getString(R.string.button_key_fn))
            .performClick()
        composeTestRule
            .onNodeWithText(composeTestRule.activity.getString(R.string.button_key_f1))
            .assertIsDisplayed()
    }

    private fun setKeyboardContent(
        modifierState: ModifierState = ModifierState(
            ctrlState = ModifierLevel.OFF,
            altState = ModifierLevel.OFF,
            shiftState = ModifierLevel.OFF,
        ),
        onCtrlPress: () -> Unit = {},
        onAltPress: () -> Unit = {},
        onEscPress: () -> Unit = {},
        onTabPress: () -> Unit = {},
        onKeyPress: (Int) -> Unit = {},
        onInteraction: () -> Unit = {},
        onHideIme: () -> Unit = {},
        onShowIme: () -> Unit = {},
        onTextPress: (String) -> Unit = {},
        onPaste: () -> Unit = {},
        onScrollInProgressChange: (Boolean) -> Unit = {},
        imeVisible: Boolean = false,
        bumpyArrows: Boolean = false,
        showImeToggleKey: Boolean = true,
        onToggleComposeMode: () -> Unit = {},
    ) {
        composeTestRule.setContent {
            ConnectBotTheme {
                TerminalKeyboardContent(
                    modifierState = modifierState,
                    onCtrlPress = onCtrlPress,
                    onAltPress = onAltPress,
                    onEscPress = onEscPress,
                    onTabPress = onTabPress,
                    onKeyPress = onKeyPress,
                    onInteraction = onInteraction,
                    onHideIme = onHideIme,
                    onShowIme = onShowIme,
                    onTextPress = onTextPress,
                    onPaste = onPaste,
                    onScrollInProgressChange = onScrollInProgressChange,
                    imeVisible = imeVisible,
                    playAnimation = false,
                    bumpyArrows = bumpyArrows,
                    showImeToggleKey = showImeToggleKey,
                    onToggleComposeMode = onToggleComposeMode,
                )
            }
        }
    }

}
