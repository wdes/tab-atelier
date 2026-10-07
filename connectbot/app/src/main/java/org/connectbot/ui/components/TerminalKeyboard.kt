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

package org.connectbot.ui.components

import android.view.HapticFeedbackConstants
import android.view.ViewConfiguration
import androidx.compose.animation.core.tween
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Keyboard
import androidx.compose.material.icons.filled.KeyboardArrowDown
import androidx.compose.material.icons.filled.KeyboardArrowUp
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowLeft
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowRight
import androidx.compose.material.icons.automirrored.filled.KeyboardReturn
import androidx.compose.material.icons.filled.ContentPaste
import androidx.compose.material.icons.filled.KeyboardHide
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.RectangleShape
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.tooling.preview.Preview
import androidx.compose.ui.unit.dp
import androidx.preference.PreferenceManager
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import org.connectbot.R
import org.connectbot.service.ModifierLevel
import org.connectbot.service.ModifierState
import org.connectbot.service.TerminalBridge
import org.connectbot.service.TerminalKeyListener
import org.connectbot.terminal.VTermKey
import org.connectbot.util.PreferenceConstants

private const val UI_OPACITY = 0.5f

/**
 * Height of the virtual keyboard keys in dp.
 */
const val TERMINAL_KEYBOARD_HEIGHT_DP = 30

/**
 * The bar is two rows of keys, so the bar is twice one key's height.
 *
 * A fixed height rather than something derived from the viewport: how much of
 * the screen the terminal gets is decided by the keyboard being up or not, not
 * by how tall this bar is.
 */
private const val TERMINAL_KEYBOARD_ROWS = 2

/**
 * Width of the virtual keyboard keys in dp.
 */
private const val TERMINAL_KEYBOARD_WIDTH_DP = 45

/**
 * Size of the content (icons and text) for the virtual keyboard keys in dp.
 */
private const val TERMINAL_KEYBOARD_CONTENT_SIZE_DP = 20

/**
 * Virtual keyboard with terminal special keys (Ctrl, Esc, arrows, function keys, etc.)
 * Positioned at bottom of console screen, horizontally scrollable
 * Auto-hide timer is managed by parent ConsoleScreen
 */
@Composable
fun TerminalKeyboard(
    bridge: TerminalBridge,
    onInteraction: () -> Unit,
    modifier: Modifier = Modifier,
    onHideIme: () -> Unit = {},
    onShowIme: () -> Unit = {},
    onPaste: () -> Unit = {},
    onScrollInProgressChange: (Boolean) -> Unit = {},
    imeVisible: Boolean = false,
    playAnimation: Boolean = false,
    isComposeModeActive: Boolean = false,
    onToggleComposeMode: () -> Unit = {},
    onShortcutModifierChange: () -> Unit = {},
    // Changed for Tab Atelier Remote (Apache-2.0 section 4(b)): the session's own
    // background, so the bar matches the terminal above it. Null keeps the theme's
    // surface, which is what every transport without a colour of its own gets.
    barColor: Color? = null,
) {
    val context = LocalContext.current
    val prefs = remember { PreferenceManager.getDefaultSharedPreferences(context) }
    val keyHandler = bridge.keyHandler
    val modifierState by keyHandler.modifierState.collectAsState()
    val bumpyArrows by remember {
        mutableStateOf(prefs.getBoolean(PreferenceConstants.BUMPY_ARROWS, false))
    }

    TerminalKeyboardContent(
        modifierState = modifierState,
        onCtrlPress = {
            keyHandler.metaPress(TerminalKeyListener.CTRL_ON, true)
            onShortcutModifierChange()
            onInteraction()
        },
        onAltPress = {
            keyHandler.metaPress(TerminalKeyListener.ALT_ON, true)
            onShortcutModifierChange()
            onInteraction()
        },
        // Changed for Tab Atelier Remote (Apache-2.0 section 4(b)): Shift, which
        // upstream's bar never had a key for — so `Shift`+`Tab` could only be typed on
        // a hardware keyboard, and never one-handed on a phone.
        //
        // `metaPress(…, true)` is the same call Ctrl and Alt make, and it cycles the
        // modifier through OFF, TRANSIENT and LOCKED. LOCKED is what makes the request
        // work one-handed: Shift is latched by one press, `Tab` is pressed separately,
        // and Shift stays on until it is pressed again. That is also what upstream
        // calls `ModifierLevel.LOCKED`, and what a French keyboard paints as
        // "Verr Maj".
        onShiftPress = {
            keyHandler.metaPress(TerminalKeyListener.SHIFT_ON, true)
            onShortcutModifierChange()
            onInteraction()
        },
        onEscPress = {
            keyHandler.sendEscape()
            onInteraction()
        },
        onTabPress = {
            keyHandler.sendTab()
            onInteraction()
        },
        onKeyPress = { key ->
            keyHandler.sendPressedKey(key)
            onInteraction()
        },
        onInteraction = onInteraction,
        onHideIme = onHideIme,
        onShowIme = onShowIme,
        // The bar sends literal text for the keys that have no key code
        // (`/` and `-`), which is the same path paste uses.
        onTextPress = { text -> bridge.injectString(text) },
        onPaste = onPaste,
        onScrollInProgressChange = onScrollInProgressChange,
        imeVisible = imeVisible,
        playAnimation = playAnimation,
        bumpyArrows = bumpyArrows,
        isComposeModeActive = isComposeModeActive,
        onToggleComposeMode = onToggleComposeMode,
        barColor = barColor,
        modifier = modifier,
    )
}

/**
 * Stateless UI component for the terminal keyboard.
 * Separated from [TerminalKeyboard] to enable preview without TerminalBridge dependency.
 */
@Composable
internal fun TerminalKeyboardContent(
    modifierState: ModifierState,
    onCtrlPress: () -> Unit,
    // Changed for Tab Atelier Remote (Apache-2.0 section 4(b)): the Shift key.
    // Defaulted so the previews that pass only some of these keep compiling; the bar
    // always supplies it.
    onShiftPress: () -> Unit = {},
    onAltPress: () -> Unit,
    onEscPress: () -> Unit,
    onTabPress: () -> Unit,
    onKeyPress: (Int) -> Unit,
    onInteraction: () -> Unit,
    onHideIme: () -> Unit,
    onShowIme: () -> Unit,
    onTextPress: (String) -> Unit,
    onPaste: () -> Unit,
    onScrollInProgressChange: (Boolean) -> Unit,
    imeVisible: Boolean,
    playAnimation: Boolean,
    bumpyArrows: Boolean,
    isComposeModeActive: Boolean = false,
    onToggleComposeMode: () -> Unit = {},
    barColor: Color? = null,
    modifier: Modifier = Modifier,
) {
    val scrollState = rememberScrollState()
    // Which page of keys is showing. Saved, so a rotation does not silently put
    // the user back on the main page in the middle of a command.
    var showFunctionKeys by rememberSaveable { mutableStateOf(false) }
    val currentOnScrollInProgressChange by rememberUpdatedState(onScrollInProgressChange)
    val view = LocalView.current

    if (bumpyArrows) {
        view.isHapticFeedbackEnabled = true
    }

    // Notify parent when scroll state changes
    LaunchedEffect(scrollState.isScrollInProgress) {
        currentOnScrollInProgressChange(scrollState.isScrollInProgress)
    }

    // Auto-scroll animation on first appearance (only if playAnimation is true)
    LaunchedEffect(playAnimation) {
        if (playAnimation) {
            // Wait a moment for layout to complete
            delay(100)

            // Scroll all the way to the right to show all keys
            scrollState.animateScrollTo(
                value = scrollState.maxValue,
                animationSpec = tween(durationMillis = 500),
            )

            // Then scroll back to the left
            delay(300)
            scrollState.animateScrollTo(
                value = 0,
                animationSpec = tween(durationMillis = 500),
            )
        }
    }

    Surface(
        modifier = modifier
            .pointerInput(Unit) {
                // Reset timer on any touch interaction
                detectTapGestures(
                    onPress = {
                        onInteraction()
                        tryAwaitRelease()
                    },
                )
            },
        // Changed for Tab Atelier Remote (Apache-2.0 section 4(b)): the bar takes the
        // tab's own background, so the strip along the bottom matches the session it
        // belongs to instead of the theme's surface. tonalElevation is dropped to
        // zero with it, because elevation tints the colour — a tab's background
        // should be the colour the daemon sent, not that colour plus a shade.
        //
        // Passing the colour to Surface also sets the content colour from it, via
        // Material's own contrast rule, so the key labels stay legible on any
        // background the daemon reports rather than assuming the theme's on-surface.
        color = barColor ?: MaterialTheme.colorScheme.surface.copy(alpha = UI_OPACITY),
        tonalElevation = if (barColor != null) 0.dp else 8.dp,
    ) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .height((TERMINAL_KEYBOARD_HEIGHT_DP * TERMINAL_KEYBOARD_ROWS).dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            // The whole key area is ONE scroll surface with both rows inside it, so
            // the rows scroll together. Two independently scrolling strips would
            // drift apart and leave the grid ragged.
            Column(modifier = Modifier.weight(7f)) {
                // ---- top row: ESC / - HOME ↑ END PGPREV, or F1..F6 ----
                Row(
                    modifier = Modifier
                        .fillMaxWidth(),
                    horizontalArrangement = Arrangement.Start, // No spacing between keys
                ) {
                    if (showFunctionKeys) {
                        for (i in 1..6) {
                            KeyButton(
                                modifier = Modifier.weight(1f),
                                text = stringResource(FUNCTION_KEY_LABELS[i - 1]),
                                onClick = { onKeyPress(functionKeyCode(i)) },
                            )
                        }
                        // No spacer after them: the six function keys share the whole
                        // row between them.
                        //
                        // One was here to hold the F pages to the main page's grid —
                        // six keys under seven columns — and it read as a missing key
                        // rather than as alignment. A key that is absent is noticed; a
                        // gap that means "aligned" is not.
                    } else {
                        KeyButton(
                            modifier = Modifier.weight(1f),
                            text = stringResource(R.string.button_key_esc),
                            contentDescription = stringResource(R.string.image_description_send_escape_character),
                            onClick = onEscPress,
                        )
                        // Literal characters, not key codes: neither exists as a
                        // VTermKey, and both glyphs are the same in every language,
                        // so there is nothing for a translator to translate.
                        // Changed for Tab Atelier Remote (Apache-2.0 section 4(b)):
                        // every key in a row carries a weight, and these two are the
                        // reason that rule has to hold for all of them rather than most.
                        //
                        // A key's content fills whatever it is given, so an unweighted
                        // key in a bounded row takes the WHOLE row — leaving zero width
                        // for the keys beside it. `Esc` measured at 0×0 because of these
                        // two: present, enabled, labelled, unpressable. A single key
                        // missing its weight breaks its neighbours, not itself, which is
                        // what made it hard to see.
                        KeyButton(modifier = Modifier.weight(1f), text = "/", onClick = { onTextPress("/") })
                        KeyButton(modifier = Modifier.weight(1f), text = "-", onClick = { onTextPress("-") })
                        KeyButton(
                            modifier = Modifier.weight(1f),
                            text = stringResource(R.string.button_key_home),
                            onClick = { onKeyPress(VTermKey.HOME) },
                        )
                        RepeatableKeyButton(
                            modifier = Modifier.weight(1f),
                            icon = Icons.Default.KeyboardArrowUp,
                            contentDescription = stringResource(R.string.image_description_up),
                            onPress = { onKeyPress(VTermKey.UP) },
                        )
                        KeyButton(
                            modifier = Modifier.weight(1f),
                            text = stringResource(R.string.button_key_end),
                            onClick = { onKeyPress(VTermKey.END) },
                        )
                        KeyButton(
                            modifier = Modifier.weight(1f),
                            text = stringResource(R.string.button_key_pgup),
                            onClick = { onKeyPress(VTermKey.PAGEUP) },
                        )
                    }
                }

                // ---- bottom row: TAB CTRL ALT ← ↓ → PGNEXT, or F7..F12 ----
                Row(
                    modifier = Modifier
                        .fillMaxWidth(),
                    horizontalArrangement = Arrangement.Start, // No spacing between keys
                ) {
                    if (showFunctionKeys) {
                        for (i in 7..12) {
                            KeyButton(
                                modifier = Modifier.weight(1f),
                                text = stringResource(FUNCTION_KEY_LABELS[i - 1]),
                                onClick = { onKeyPress(functionKeyCode(i)) },
                            )
                        }
                    } else {
                        KeyButton(
                            modifier = Modifier.weight(1f),
                            text = stringResource(R.string.button_key_tab),
                            contentDescription = stringResource(R.string.image_description_send_tab_character),
                            onClick = onTabPress,
                        )
                        // Ctrl key (sticky modifier)
                        ModifierKeyButton(
                            modifier = Modifier.weight(1f),
                            text = stringResource(R.string.button_key_ctrl),
                            contentDescription = stringResource(R.string.image_description_toggle_control_character),
                            modifierLevel = modifierState.ctrlState,
                            onClick = onCtrlPress,
                        )
                        // Alt key (sticky modifier)
                        ModifierKeyButton(
                            modifier = Modifier.weight(1f),
                            text = stringResource(R.string.button_key_alt),
                            contentDescription = stringResource(R.string.image_description_toggle_alt_key),
                            modifierLevel = modifierState.altState,
                            onClick = onAltPress,
                        )
                        // Shift key (sticky modifier). A press latches it, so Shift then
                        // Tab is two taps with one hand rather than a held key — which is
                        // the whole point of having it here. `modifierState.shiftState`
                        // drives the same OFF/TRANSIENT/LOCKED indicator the other two
                        // modifiers use.
                        ModifierKeyButton(
                            modifier = Modifier.weight(1f),
                            text = stringResource(R.string.button_key_shift),
                            contentDescription = stringResource(R.string.image_description_toggle_shift),
                            modifierLevel = modifierState.shiftState,
                            onClick = onShiftPress,
                        )
                        // Arrow keys (repeatable). Left and right are auto-mirrored:
                        // where they point is a property of the arrow, not of the
                        // reading direction.
                        RepeatableKeyButton(
                            modifier = Modifier.weight(1f),
                            icon = Icons.AutoMirrored.Filled.KeyboardArrowLeft,
                            contentDescription = stringResource(R.string.image_description_left),
                            onPress = { onKeyPress(VTermKey.LEFT) },
                        )
                        RepeatableKeyButton(
                            modifier = Modifier.weight(1f),
                            icon = Icons.Default.KeyboardArrowDown,
                            contentDescription = stringResource(R.string.image_description_down),
                            onPress = { onKeyPress(VTermKey.DOWN) },
                        )
                        RepeatableKeyButton(
                            modifier = Modifier.weight(1f),
                            icon = Icons.AutoMirrored.Filled.KeyboardArrowRight,
                            contentDescription = stringResource(R.string.image_description_right),
                            onPress = { onKeyPress(VTermKey.RIGHT) },
                        )
                        KeyButton(
                            modifier = Modifier.weight(1f),
                            text = stringResource(R.string.button_key_pgdn),
                            onClick = { onKeyPress(VTermKey.PAGEDOWN) },
                        )
                    }
                }
            }

            // ---- trailing column: one button per row ----
            // Changed for Tab Atelier Remote (Apache-2.0 section 4(b)): these keys take
            // their WIDTH from the column and their height from KeyButton, and neither
            // is a weight — which is the opposite of the keys column beside it, and the
            // reason is the axis.
            //
            // A weighted child divides the space left over on its axis, and this column
            // has no fixed height: it is as tall as its two keys, so a height weight has
            // no space to divide and the keys measure at zero. They were then present,
            // enabled, labelled and unpressable — the worst way for a control to be
            // missing, and exactly what the two failing tests reported.
            //
            // `fillMaxHeight()` is not the answer either, and trying it made the bar
            // full-screen: the column took the whole available height, so the bar grew
            // to cover the terminal and pushed the keys column — 'Esc' among them — out
            // of view. `fillMaxWidth()` is right because width is the axis the ROW
            // bounds, and the height is already fixed inside KeyButton.
            Column(modifier = Modifier.weight(1f)) {
                if (showFunctionKeys) {
                    // Back to the keys. A return arrow rather than a plain back
                    // arrow: this goes back a *page* of this bar, not out of the
                    // session.
                    KeyButton(
                        modifier = Modifier.fillMaxWidth(),
                        icon = Icons.AutoMirrored.Filled.KeyboardReturn,
                        contentDescription = stringResource(R.string.image_description_back_to_keys),
                        onClick = { showFunctionKeys = false },
                    )
                } else {
                    KeyButton(
                        modifier = Modifier.fillMaxWidth(),
                        text = stringResource(R.string.button_key_fn),
                        contentDescription = stringResource(R.string.image_description_function_keys),
                        onClick = { showFunctionKeys = true },
                    )
                }

                if (showFunctionKeys) {
                    KeyButton(
                        modifier = Modifier.fillMaxWidth(),
                        icon = Icons.Default.ContentPaste,
                        contentDescription = stringResource(R.string.console_menu_paste),
                        onClick = onPaste,
                    )
                } else {
                    // The optional soft-keyboard key. It shows or hides the IME
                    // according to which way the IME currently is, rather than being
                    // one toggle: the two actions are not symmetric, and the icon
                    // says which one this press will do.
                    //
                    // It reports the interaction as well as the press, which the old
                    // bar's equivalent did and which matters here: the console resets
                    // its auto-hide timer on an interaction, so a key that does not
                    // report one can disappear from under the user mid-press. That is
                    // the behaviour the existing test pins, and dropping it was a
                    // regression the test caught.
                    if (imeVisible) {
                        KeyButton(
                            modifier = Modifier.fillMaxWidth(),
                            icon = Icons.Default.KeyboardHide,
                            contentDescription = stringResource(R.string.image_description_hide_keyboard),
                            onClick = {
                                onHideIme()
                                onInteraction()
                            },
                        )
                    } else {
                        KeyButton(
                            modifier = Modifier.fillMaxWidth(),
                            icon = Icons.Default.Keyboard,
                            contentDescription = stringResource(R.string.image_description_show_keyboard),
                            onClick = {
                                onShowIme()
                                onInteraction()
                            },
                        )
                    }
                }
            }
        }
    }
}

/**
 * A button for single-press keys (Ctrl, Esc, Tab, Home, End, PgUp, PgDn, F1-F12)
 * Styled to match the old keyboard layout: rectangular 45dp × 30dp, unbordered
 */
/**
 * The key code for function key [index], one-based.
 *
 * A table because there is no arithmetic relationship between the twelve constants
 * and their numbers: they are separate enum entries, and a `when` over twelve of
 * them would be the same list written sideways. The bounds are not defended because
 * the only caller is a `for (i in 1..12)` in this file.
 */
private val FUNCTION_KEY_CODES = intArrayOf(
    VTermKey.FUNCTION_1,
    VTermKey.FUNCTION_2,
    VTermKey.FUNCTION_3,
    VTermKey.FUNCTION_4,
    VTermKey.FUNCTION_5,
    VTermKey.FUNCTION_6,
    VTermKey.FUNCTION_7,
    VTermKey.FUNCTION_8,
    VTermKey.FUNCTION_9,
    VTermKey.FUNCTION_10,
    VTermKey.FUNCTION_11,
    VTermKey.FUNCTION_12,
)

private fun functionKeyCode(index: Int): Int = FUNCTION_KEY_CODES[index - 1]

/**
 * The twelve function keys' labels, from upstream's own strings.
 *
 * Changed for Tab Atelier Remote (Apache-2.0 section 4(b)): this fork had a single
 * `translatable="false"` format string for these, and upstream's twelve are already
 * translated — identically in every locale it ships, because a function key is `F1`
 * everywhere, but using theirs means one less string of our own and a label that
 * follows theirs if it ever changes.
 *
 * A table rather than `stringResource` of a computed id, because a resource id cannot be
 * built from an index, and a `when` over twelve of them would be this list written
 * sideways — the same shape, and the same reason, as [FUNCTION_KEY_CODES] above.
 */
private val FUNCTION_KEY_LABELS = intArrayOf(
    R.string.button_key_f1,
    R.string.button_key_f2,
    R.string.button_key_f3,
    R.string.button_key_f4,
    R.string.button_key_f5,
    R.string.button_key_f6,
    R.string.button_key_f7,
    R.string.button_key_f8,
    R.string.button_key_f9,
    R.string.button_key_f10,
    R.string.button_key_f11,
    R.string.button_key_f12,
)

@Composable
private fun KeyButton(
    // Changed for Tab Atelier Remote (Apache-2.0 section 4(b)): optional, because
    // a button's accessible name does not have to be a separate string. When the
    // key shows a label, that label IS the name — a screen reader reads the Text,
    // and a second description would either duplicate it or contradict it. It is
    // required in spirit for an icon-only key, where there is no text to read, and
    // every such call site passes one.
    contentDescription: String? = null,
    modifier: Modifier = Modifier,
    text: String? = null,
    icon: ImageVector? = null,
    onClick: (() -> Unit)? = null,
    backgroundColor: Color = MaterialTheme.colorScheme.surface.copy(alpha = UI_OPACITY),
    tint: Color = MaterialTheme.colorScheme.onSurface,
) {
    // Changed for Tab Atelier Remote (Apache-2.0 section 4(b)): the WIDTH comes from
    // the caller, the height from here.
    //
    // It used to be a hardcoded `.size(width = TERMINAL_KEYBOARD_WIDTH_DP, height = …)`,
    // and that is what made the bar's trailing keys unreachable: a fixed width beats
    // the `Modifier.weight(1f)` a caller passes, so a row of seven keys stayed seven
    // fixed widths wide however narrow the phone was, and `FN` and the keyboard toggle
    // — laid out after them — fell outside the window. Present, enabled, labelled, and
    // impossible to see or press, which is the worst way for a control to be missing.
    //
    // Only the height is fixed here now. A caller that wants an even share of the row
    // passes `weight(1f)`; one that wants a natural width passes nothing. The keys stay
    // touch-sized in height either way, which is the dimension that matters for a
    // target.
    val surfaceModifier = modifier
        .height(TERMINAL_KEYBOARD_HEIGHT_DP.dp)

    val content: @Composable () -> Unit = {
        Box(
            contentAlignment = Alignment.Center,
            modifier = Modifier.fillMaxSize(),
        ) {
            if (text != null) {
                // Changed for Tab Atelier Remote (Apache-2.0 section 4(b)): a text key
                // uses the description it is given.
                //
                // It used not to. The parameter was only read in the icon branch, so
                // passing one with a text key dropped it in silence — `FN` had no
                // accessible name at all, and a screen reader announced the two
                // letters rather than "Show function keys". Worse, the caller could not
                // tell: the call site reads as though the description is used, and only
                // a test with `assertIsDisplayed` on the description found it.
                //
                // No description still means no description: a key whose label is
                // already its name, like "Esc", is left alone rather than given an
                // empty one.
                Text(
                    text = text,
                    style = MaterialTheme.typography.labelSmall,
                    color = tint,
                    modifier = if (contentDescription != null) {
                        Modifier.semantics { this.contentDescription = contentDescription }
                    } else {
                        Modifier
                    },
                )
            } else if (icon != null) {
                Icon(
                    imageVector = icon,
                    contentDescription = contentDescription,
                    tint = tint,
                    modifier = Modifier.height(TERMINAL_KEYBOARD_CONTENT_SIZE_DP.dp),
                )
            }
        }
    }

    if (onClick != null) {
        Surface(
            onClick = onClick,
            modifier = surfaceModifier,
            shape = RectangleShape,
            color = backgroundColor,
            content = content,
        )
    } else {
        Surface(
            modifier = surfaceModifier,
            shape = RectangleShape,
            color = backgroundColor,
            content = content,
        )
    }
}

/**
 * A button for repeatable keys (arrow keys)
 * Starts repeating after initial delay when held down
 * Styled to match the old keyboard layout: rectangular 45dp × 30dp with border
 */
@Composable
private fun RepeatableKeyButton(
    icon: ImageVector,
    contentDescription: String?,
    onPress: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val coroutineScope = rememberCoroutineScope()
    var isPressed by remember { mutableStateOf(false) }
    var repeatJob by remember { mutableStateOf<Job?>(null) }

    // Cleanup on unmount
    DisposableEffect(Unit) {
        onDispose {
            repeatJob?.cancel()
        }
    }

    val backgroundColor =
        if (isPressed) {
            MaterialTheme.colorScheme.primaryContainer.copy(alpha = UI_OPACITY)
        } else {
            MaterialTheme.colorScheme.surface.copy(alpha = UI_OPACITY)
        }

    KeyButton(
        icon = icon,
        contentDescription = contentDescription,
        onClick = null,
        modifier = modifier.pointerInput(Unit) {
            detectTapGestures(
                onPress = {
                    isPressed = true
                    var sentPress = false

                    // Start a job that handles initial delay, first press, and repeat
                    val tapTimeout = ViewConfiguration.getTapTimeout().toLong()
                    repeatJob = coroutineScope.launch {
                        // Delay before first press to allow scroll gestures to steal touch
                        delay(tapTimeout)
                        if (!isPressed) return@launch

                        // First press after initial tap delay
                        sentPress = true
                        onPress()

                        // Wait before starting repeat
                        delay(500 - tapTimeout)
                        while (isPressed) {
                            sentPress = true
                            onPress()
                            delay(50) // Repeat interval
                        }
                    }

                    // Wait for release - returns true if normal release, false if gesture stolen
                    val released = tryAwaitRelease()
                    isPressed = false

                    if (released && !sentPress) {
                        // User released but key hasn't been sent yet (quick tap) - send it now
                        repeatJob?.cancel()
                        onPress()
                    } else {
                        repeatJob?.cancel()
                    }
                },
            )
        },
        backgroundColor = backgroundColor,
    )
}

@Composable
private fun ModifierKeyButton(
    text: String,
    contentDescription: String?,
    modifierLevel: ModifierLevel,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val backgroundColor = when (modifierLevel) {
        ModifierLevel.OFF -> MaterialTheme.colorScheme.surface.copy(alpha = UI_OPACITY)
        ModifierLevel.TRANSIENT -> MaterialTheme.colorScheme.primaryContainer.copy(alpha = 0.7f)
        ModifierLevel.LOCKED -> MaterialTheme.colorScheme.primary.copy(alpha = 0.8f)
    }

    val textColor = when (modifierLevel) {
        ModifierLevel.OFF -> MaterialTheme.colorScheme.onSurface
        ModifierLevel.TRANSIENT -> MaterialTheme.colorScheme.onPrimaryContainer
        ModifierLevel.LOCKED -> MaterialTheme.colorScheme.onPrimary
    }

    KeyButton(
        text = text,
        contentDescription = contentDescription,
        onClick = onClick,
        modifier = modifier,
        backgroundColor = backgroundColor,
        tint = textColor,
    )
}

@Preview(name = "Terminal Keyboard - Default State", showBackground = true)
@Composable
private fun TerminalKeyboardPreview() {
    MaterialTheme {
        TerminalKeyboardContent(
            modifierState = ModifierState(
                ctrlState = ModifierLevel.OFF,
                altState = ModifierLevel.OFF,
                shiftState = ModifierLevel.OFF,
            ),
            onCtrlPress = {},
            onAltPress = {},
            onEscPress = {},
            onTabPress = {},
            onKeyPress = {},
            onInteraction = {},
            onHideIme = {},
            onShowIme = {},
            onTextPress = {},
            onPaste = {},
            onScrollInProgressChange = {},
            imeVisible = false,
            playAnimation = false,
            bumpyArrows = false,
        )
    }
}

@Preview(name = "Terminal Keyboard - Ctrl Pressed", showBackground = true)
@Composable
private fun TerminalKeyboardCtrlPressedPreview() {
    MaterialTheme {
        TerminalKeyboardContent(
            modifierState = ModifierState(
                ctrlState = ModifierLevel.TRANSIENT,
                altState = ModifierLevel.OFF,
                shiftState = ModifierLevel.OFF,
            ),
            onCtrlPress = {},
            onAltPress = {},
            onEscPress = {},
            onTabPress = {},
            onKeyPress = {},
            onInteraction = {},
            onHideIme = {},
            onShowIme = {},
            onTextPress = {},
            onPaste = {},
            onScrollInProgressChange = {},
            imeVisible = false,
            playAnimation = false,
            bumpyArrows = false,
        )
    }
}

@Preview(name = "Terminal Keyboard - Ctrl Locked", showBackground = true)
@Composable
private fun TerminalKeyboardCtrlLockedPreview() {
    MaterialTheme {
        TerminalKeyboardContent(
            modifierState = ModifierState(
                ctrlState = ModifierLevel.LOCKED,
                altState = ModifierLevel.OFF,
                shiftState = ModifierLevel.OFF,
            ),
            onCtrlPress = {},
            onAltPress = {},
            onEscPress = {},
            onTabPress = {},
            onKeyPress = {},
            onInteraction = {},
            onHideIme = {},
            onShowIme = {},
            onTextPress = {},
            onPaste = {},
            onScrollInProgressChange = {},
            imeVisible = false,
            playAnimation = false,
            bumpyArrows = false,
        )
    }
}

@Preview(name = "Terminal Keyboard - IME Visible", showBackground = true)
@Composable
private fun TerminalKeyboardImeVisiblePreview() {
    MaterialTheme {
        TerminalKeyboardContent(
            modifierState = ModifierState(
                ctrlState = ModifierLevel.OFF,
                altState = ModifierLevel.OFF,
                shiftState = ModifierLevel.OFF,
            ),
            onCtrlPress = {},
            onAltPress = {},
            onEscPress = {},
            onTabPress = {},
            onKeyPress = {},
            onInteraction = {},
            onHideIme = {},
            onShowIme = {},
            onTextPress = {},
            onPaste = {},
            onScrollInProgressChange = {},
            imeVisible = true,
            playAnimation = false,
            bumpyArrows = false,
        )
    }
}
