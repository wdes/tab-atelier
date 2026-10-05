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
import androidx.test.ext.junit.runners.AndroidJUnit4
import dagger.hilt.android.testing.HiltAndroidRule
import dagger.hilt.android.testing.HiltAndroidTest
import org.connectbot.ui.screens.help.HelpScreen
import org.connectbot.ui.theme.ConnectBotTheme
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith

@HiltAndroidTest
@RunWith(AndroidJUnit4::class)
class HelpScreenTest {
    @get:Rule(order = 0)
    val hiltRule = HiltAndroidRule(this)

    @get:Rule(order = 1)
    val composeTestRule = createAndroidComposeRule<HiltComponentActivity>()

    @Before
    fun setUp() {
        hiltRule.inject()
    }

    @Test
    fun helpScreen_displaysTitle() {
        composeTestRule.setContent {
            ConnectBotTheme {
                HelpScreen(
                    onNavigateBack = {},
                    onNavigateToHints = {},
                    onNavigateToEula = {},
                )
            }
        }

        composeTestRule
            .onNodeWithText("Help")
            .assertIsDisplayed()
    }

    @Test
    fun helpScreen_hasBackButton() {
        var backCalled = false

        composeTestRule.setContent {
            ConnectBotTheme {
                HelpScreen(
                    onNavigateBack = { backCalled = true },
                    onNavigateToHints = {},
                    onNavigateToEula = {},
                )
            }
        }

        composeTestRule
            .onNodeWithContentDescription("Navigate up")
            .performClick()

        assertTrue(backCalled)
    }

    @Test
    fun helpScreen_displaysAboutSection() {
        composeTestRule.setContent {
            ConnectBotTheme {
                HelpScreen(
                    onNavigateBack = {},
                    onNavigateToHints = {},
                    onNavigateToEula = {},
                )
            }
        }

        composeTestRule
            .onNodeWithText("About")
            .assertIsDisplayed()
    }

    @Test
    fun helpScreen_hintsItemNavigates() {
        var hintsCalled = false

        composeTestRule.setContent {
            ConnectBotTheme {
                HelpScreen(
                    onNavigateBack = {},
                    onNavigateToHints = { hintsCalled = true },
                    onNavigateToEula = {},
                )
            }
        }

        composeTestRule
            .onNodeWithText("Hints")
            .performClick()

        assertTrue(hintsCalled)
    }

    @Test
    fun helpScreen_eulaItemNavigates() {
        var eulaCalled = false

        composeTestRule.setContent {
            ConnectBotTheme {
                HelpScreen(
                    onNavigateBack = {},
                    onNavigateToHints = {},
                    onNavigateToEula = { eulaCalled = true },
                )
            }
        }

        composeTestRule
            .onNodeWithText("Terms & Conditions")
            .performClick()

        assertTrue(eulaCalled)
    }

    // Commented out for Tab Atelier Remote (Apache-2.0 section 4(b)): the
    // Contact button this drives is commented out of HelpScreen itself, for the
    // same reason — it leads to ConnectBot's own community (their IRC channels
    // and mailing list), which is the wrong place to send our users. Restore
    // both together, or neither.
    //
    // @Test
    // fun helpScreen_contactItemNavigates() {
    //     var contactCalled = false
    //
    //     composeTestRule.setContent {
    //         ConnectBotTheme {
    //             HelpScreen(
    //                 onNavigateBack = {},
    //                 onNavigateToHints = {},
    //                 onNavigateToEula = {},
    //                 onNavigateToContact = { contactCalled = true },
    //             )
    //         }
    //     }
    //
    //     composeTestRule
    //         .onNodeWithText("Contact & Support")
    //         .performClick()
    //
    //     assertTrue(contactCalled)
    // }

    // Added for Tab Atelier Remote: the About screen's source-code buttons are
    // ours, and until this existed the only assertion about that screen's
    // buttons was the commented-out one above — so a fork that quietly lost its
    // provenance links would still have passed.
    //
    // assertExists rather than assertIsDisplayed: the About section is a column
    // inside a LazyColumn item, and these buttons sit below the fold on a
    // phone-sized viewport. Being in the tree is the property worth pinning;
    // how far down they appear is layout, not provenance.
    @Test
    fun helpScreen_sourceCodeItemsAreShown() {
        composeTestRule.setContent {
            ConnectBotTheme {
                HelpScreen(
                    onNavigateBack = {},
                    onNavigateToHints = {},
                    onNavigateToEula = {},
                )
            }
        }

        composeTestRule.onNodeWithText("Tab Atelier source code").assertExists()
        composeTestRule.onNodeWithText("ConnectBot source code").assertExists()
    }
}
