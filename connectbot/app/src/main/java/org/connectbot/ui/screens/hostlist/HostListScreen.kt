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

package org.connectbot.ui.screens.hostlist

import android.widget.Toast
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.CheckCircle
import androidx.compose.material.icons.filled.Computer
import androidx.compose.material.icons.filled.ContentCopy
import androidx.compose.material.icons.filled.Delete
import androidx.compose.material.icons.filled.Edit
import androidx.compose.material.icons.filled.Error
import androidx.compose.material.icons.filled.ExpandLess
import androidx.compose.material.icons.filled.ExpandMore
import androidx.compose.material.icons.filled.Key
import androidx.compose.material.icons.filled.Link
import androidx.compose.material.icons.filled.LinkOff
import androidx.compose.material.icons.filled.Lock
import androidx.compose.material.icons.filled.MoreVert
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material.icons.filled.Terminal
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FloatingActionButton
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.ListItem
import androidx.compose.material3.ListItemDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarDuration
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.SnackbarResult
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.colorResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.core.graphics.toColorInt
import androidx.hilt.lifecycle.viewmodel.compose.hiltViewModel
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.connectbot.R
import org.connectbot.data.JsonImportReader
import org.connectbot.data.JsonImportTooLargeException
import org.connectbot.data.entity.Host
import org.connectbot.data.entity.Pubkey
import org.connectbot.tabatelier.TabAtelierTab
import org.connectbot.transport.TabAtelier
import org.connectbot.ui.LocalTerminalManager
import org.connectbot.ui.PreviewScreen
import org.connectbot.ui.components.DisconnectAllDialog
import org.connectbot.ui.components.ShortcutCustomizationDialog
import org.connectbot.ui.components.TextInputAlertDialog
import org.connectbot.ui.theme.ConnectBotTheme
import org.connectbot.util.IconStyle
import java.io.IOException

internal object HostListTestTags {
    fun itemRow(hostId: Long): String = "host_item_${hostId}_row"
    fun itemMenuButton(hostId: Long): String = "host_item_${hostId}_menu_button"

    // Changed for Tab Atelier Remote (Apache-2.0 section 4(b)).
    fun tabStatus(hostId: Long): String = "host_item_${hostId}_tab_status"

    /**
     * One of a tab-atelier host's tabs, as a row.
     *
     * Added for Tab Atelier Remote: tapping a tab opens that tab's session, and
     * the tab id has to reach the navigation for that to work. Without a stable
     * tag there is no way to assert it does.
     */
    fun tabRow(hostId: Long, tabId: String): String = "host_item_${hostId}_tab_${tabId}_row"

    /**
     * The button that shows and hides a tab-atelier host's tab list.
     *
     * Tagged so a test can assert the control exists and is clickable: it began
     * as a bare `Icon`, which is drawn but has no click action, so it could not
     * be activated and was not announced as a button.
     */
    fun itemExpandButton(hostId: Long): String = "host_item_${hostId}_expand_button"
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun HostListScreen(
    onNavigateToConsole: (Host, String?) -> Unit,
    onNavigateToEditHost: (Host?) -> Unit,
    onNavigateToSettings: () -> Unit,
    onNavigateToPubkeys: () -> Unit,
    onNavigateToPortForwards: (Host) -> Unit,
    onNavigateToProfiles: () -> Unit,
    onNavigateToHelp: () -> Unit,
    modifier: Modifier = Modifier,
    onNavigateToSettingsHighlightConnPersist: () -> Unit = {},
    makingShortcut: Boolean = false,
    onSelectShortcut: (Host, String?, IconStyle) -> Unit = { _, _, _ -> },
    shouldShowNotificationWarning: () -> Boolean = { false },
    onNotificationSnackbarFinish: () -> Unit = {},
    viewModel: HostListViewModel = hiltViewModel(),
) {
    val context = LocalContext.current
    val terminalManager = LocalTerminalManager.current

    LaunchedEffect(terminalManager) {
        terminalManager?.let { viewModel.setTerminalManager(it) }
    }

    val uiState by viewModel.uiState.collectAsState()
    val scope = rememberCoroutineScope()

    // File picker for export
    val exportLauncher = rememberLauncherForActivityResult(
        contract = ActivityResultContracts.CreateDocument("application/json"),
    ) { uri ->
        if (uri != null && uiState.exportedJson != null) {
            scope.launch {
                try {
                    context.contentResolver.openOutputStream(uri)?.use { outputStream ->
                        outputStream.write(uiState.exportedJson!!.toByteArray())
                    }
                    val exportResult = uiState.exportResult
                    if (exportResult != null) {
                        Toast.makeText(
                            context,
                            context.getString(
                                R.string.export_hosts_success,
                                exportResult.hostCount,
                                exportResult.profileCount,
                            ),
                            Toast.LENGTH_SHORT,
                        ).show()
                    }
                } catch (e: Exception) {
                    Toast.makeText(
                        context,
                        context.getString(R.string.export_hosts_failed, e.message),
                        Toast.LENGTH_LONG,
                    ).show()
                }
                viewModel.clearExportedJson()
            }
        } else {
            viewModel.clearExportedJson()
        }
    }

    // File picker for import
    val importLauncher = rememberLauncherForActivityResult(
        contract = ActivityResultContracts.OpenDocument(),
    ) { uri ->
        if (uri != null) {
            scope.launch {
                try {
                    val jsonString = withContext(Dispatchers.IO) {
                        context.contentResolver.openInputStream(uri)?.use(JsonImportReader::read)
                            ?: throw IOException(context.getString(R.string.import_file_unavailable))
                    }
                    viewModel.importHosts(jsonString)
                } catch (e: JsonImportTooLargeException) {
                    Toast.makeText(context, R.string.import_json_file_too_large, Toast.LENGTH_LONG).show()
                } catch (e: Exception) {
                    Toast.makeText(
                        context,
                        context.getString(R.string.import_hosts_failed, e.message),
                        Toast.LENGTH_LONG,
                    ).show()
                }
            }
        }
    }

    // Show errors as Toast notifications
    LaunchedEffect(uiState.error) {
        uiState.error?.let { errorMessage ->
            Toast.makeText(context, errorMessage, Toast.LENGTH_LONG).show()
            viewModel.clearError()
        }
    }

    // Handle export result - launch file picker when JSON is ready
    LaunchedEffect(uiState.exportedJson) {
        if (uiState.exportedJson != null) {
            exportLauncher.launch(context.getString(R.string.export_hosts_filename))
        }
    }

    // Handle import result
    LaunchedEffect(uiState.importResult) {
        uiState.importResult?.let { result ->
            Toast.makeText(
                context,
                context.getString(
                    R.string.import_hosts_success,
                    result.hostsImported,
                    result.hostsSkipped,
                    result.profilesImported,
                    result.profilesSkipped,
                ),
                Toast.LENGTH_SHORT,
            ).show()
            viewModel.clearImportResult()
        }
    }

    var shortcutHost by remember { mutableStateOf<Host?>(null) }

    if (shortcutHost != null) {
        ShortcutCustomizationDialog(
            host = shortcutHost!!,
            onDismiss = { shortcutHost = null },
            onConfirm = { color, iconStyle ->
                onSelectShortcut(shortcutHost!!, color, iconStyle)
                shortcutHost = null
            },
        )
    }

    uiState.startupKeyPrompt?.let { pendingKey ->
        StartupKeyPasswordDialog(
            pubkey = pendingKey,
            wrongPassword = uiState.startupKeyWrongPassword,
            onDismiss = viewModel::dismissStartupKeyPrompt,
            onProvidePassword = viewModel::submitStartupKeyPassword,
        )
    }

    HostListScreenContent(
        uiState = uiState,
        makingShortcut = makingShortcut,
        onNavigateToConsole = onNavigateToConsole,
        onSelectShortcut = { host -> shortcutHost = host },
        onNavigateToEditHost = onNavigateToEditHost,
        onNavigateToSettings = onNavigateToSettings,
        onNavigateToSettingsHighlightConnPersist = onNavigateToSettingsHighlightConnPersist,
        onNavigateToPubkeys = onNavigateToPubkeys,
        onNavigateToPortForwards = onNavigateToPortForwards,
        onNavigateToProfiles = onNavigateToProfiles,
        onNavigateToHelp = onNavigateToHelp,
        onToggleSortOrder = viewModel::toggleSortOrder,
        onToggleTabHost = viewModel::toggleTabHost,
        onRefreshTabs = viewModel::refreshTabs,
        onDeleteHost = viewModel::deleteHost,
        onDuplicateHost = viewModel::duplicateHost,
        onForgetHostKeys = viewModel::forgetHostKeys,
        onDisconnectHost = viewModel::disconnectHost,
        onDisconnectAll = viewModel::disconnectAll,
        onExportHosts = viewModel::exportHosts,
        // Some document providers label JSON exports as text/plain or application/octet-stream.
        onImportHosts = { importLauncher.launch(arrayOf("*/*")) },
        shouldShowNotificationWarning = shouldShowNotificationWarning,
        onNotificationSnackbarFinish = onNotificationSnackbarFinish,
        modifier = modifier,
    )
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun HostListScreenContent(
    uiState: HostListUiState,
    onNavigateToConsole: (Host, String?) -> Unit,
    onNavigateToEditHost: (Host?) -> Unit,
    onNavigateToSettings: () -> Unit,
    onNavigateToPubkeys: () -> Unit,
    onNavigateToPortForwards: (Host) -> Unit,
    onNavigateToProfiles: () -> Unit,
    onNavigateToHelp: () -> Unit,
    onToggleSortOrder: () -> Unit,
    // Changed for Tab Atelier Remote (Apache-2.0 section 4(b)): a tab-atelier
    // host's row expands and refreshes its tabs.
    onToggleTabHost: (Long) -> Unit = {},
    onRefreshTabs: (Host) -> Unit = {},
    onDeleteHost: (Host) -> Unit,
    onDuplicateHost: (Host) -> Unit,
    onForgetHostKeys: (Host) -> Unit,
    onDisconnectHost: (Host) -> Unit,
    onDisconnectAll: () -> Unit,
    modifier: Modifier = Modifier,
    makingShortcut: Boolean = false,
    onSelectShortcut: (Host) -> Unit = {},
    onNavigateToSettingsHighlightConnPersist: () -> Unit = {},
    onExportHosts: () -> Unit = {},
    onImportHosts: () -> Unit = {},
    shouldShowNotificationWarning: () -> Boolean = { false },
    onNotificationSnackbarFinish: () -> Unit = {},
) {
    var showMenu by remember { mutableStateOf(false) }
    var showDisconnectAllDialog by remember { mutableStateOf(false) }
    val snackbarHostState = remember { SnackbarHostState() }

    // Show snackbar when there's an error
    LaunchedEffect(uiState.error) {
        uiState.error?.let { error ->
            snackbarHostState.showSnackbar(
                message = error,
                withDismissAction = true,
            )
        }
    }

    val notificationDeniedMessage = stringResource(R.string.notification_permission_denied_snackbar)
    val settingsLabel = stringResource(R.string.list_menu_settings)
    val currentShouldShowNotificationWarning by rememberUpdatedState(shouldShowNotificationWarning)
    val currentOnNavigateToSettingsHighlightConnPersist by rememberUpdatedState(onNavigateToSettingsHighlightConnPersist)
    val currentOnNotificationSnackbarFinish by rememberUpdatedState(onNotificationSnackbarFinish)

    // Show snackbar once per launch when connections won't persist in the background
    LaunchedEffect(Unit) {
        if (currentShouldShowNotificationWarning()) {
            val result = snackbarHostState.showSnackbar(
                message = notificationDeniedMessage,
                actionLabel = settingsLabel,
                withDismissAction = true,
                duration = SnackbarDuration.Long,
            )
            if (result == SnackbarResult.ActionPerformed) {
                currentOnNavigateToSettingsHighlightConnPersist()
            }
            currentOnNotificationSnackbarFinish()
        }
    }

    Scaffold(
        snackbarHost = { SnackbarHost(snackbarHostState) },
        topBar = {
            TopAppBar(
                title = { Text(stringResource(R.string.app_name)) },
                actions = {
                    if (!makingShortcut) {
                        IconButton(onClick = { showMenu = true }) {
                            Icon(Icons.Default.MoreVert, contentDescription = stringResource(R.string.button_more_options))
                        }
                        DropdownMenu(
                            expanded = showMenu,
                            onDismissRequest = { showMenu = false },
                        ) {
                            DropdownMenuItem(
                                text = {
                                    Text(
                                        stringResource(
                                            if (uiState.sortedByColor) {
                                                R.string.list_menu_sortname
                                            } else {
                                                R.string.list_menu_sortcolor
                                            },
                                        ),
                                    )
                                },
                                onClick = {
                                    showMenu = false
                                    onToggleSortOrder()
                                },
                            )
                            DropdownMenuItem(
                                text = { Text(stringResource(R.string.list_menu_settings)) },
                                onClick = {
                                    showMenu = false
                                    onNavigateToSettings()
                                },
                            )
                            DropdownMenuItem(
                                text = { Text(stringResource(R.string.profile_list_title)) },
                                onClick = {
                                    showMenu = false
                                    onNavigateToProfiles()
                                },
                            )
                            DropdownMenuItem(
                                text = { Text(stringResource(R.string.list_menu_pubkeys)) },
                                onClick = {
                                    showMenu = false
                                    onNavigateToPubkeys()
                                },
                            )
                            DropdownMenuItem(
                                text = { Text(stringResource(R.string.list_menu_export_hosts)) },
                                onClick = {
                                    showMenu = false
                                    onExportHosts()
                                },
                            )
                            DropdownMenuItem(
                                text = { Text(stringResource(R.string.list_menu_import_hosts)) },
                                onClick = {
                                    showMenu = false
                                    onImportHosts()
                                },
                            )
                            DropdownMenuItem(
                                text = { Text(stringResource(R.string.list_menu_disconnect)) },
                                onClick = {
                                    showMenu = false
                                    showDisconnectAllDialog = true
                                },
                            )
                            DropdownMenuItem(
                                text = { Text(stringResource(R.string.title_help)) },
                                onClick = {
                                    showMenu = false
                                    onNavigateToHelp()
                                },
                            )
                        }
                    }
                },
            )
        },
        floatingActionButton = {
            if (!makingShortcut) {
                FloatingActionButton(
                    onClick = { onNavigateToEditHost(null) },
                    // This matches the FloatingActionButtonMenu padding
                    modifier = Modifier.padding(end = 16.dp, bottom = 16.dp),
                ) {
                    Icon(Icons.Default.Add, contentDescription = stringResource(R.string.hostpref_add_host))
                }
            }
        },
        modifier = modifier,
    ) { padding ->
        Box(
            modifier = Modifier
                .padding(padding)
                .fillMaxSize(),
        ) {
            when {
                uiState.isLoading -> {
                    CircularProgressIndicator(
                        modifier = Modifier.align(Alignment.Center),
                    )
                }

                uiState.hosts.isEmpty() -> {
                    Column(
                        modifier = Modifier.align(Alignment.Center),
                        horizontalAlignment = Alignment.CenterHorizontally,
                    ) {
                        Text(
                            text = stringResource(R.string.empty_hosts_message),
                            style = MaterialTheme.typography.bodyLarge,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                            modifier = Modifier.padding(bottom = 8.dp),
                        )
                        TextButton(onClick = { onNavigateToEditHost(null) }) {
                            Text(stringResource(R.string.hostpref_add_host))
                        }
                    }
                }

                else -> {
                    LazyColumn(
                        modifier = Modifier.fillMaxSize(),
                        contentPadding = PaddingValues(
                            start = 16.dp,
                            end = 16.dp,
                            top = 16.dp,
                            bottom = 104.dp, // Extra padding to avoid FAB menu overlap (88dp + 16dp for menu padding)
                        ),
                        verticalArrangement = Arrangement.spacedBy(8.dp),
                    ) {
                        // Changed for Tab Atelier Remote (Apache-2.0 section
                        // 4(b)): the list is flattened into rows so a
                        // tab-atelier server's tabs can sit under it with keys
                        // of their own. A non-tab-atelier host contributes one
                        // row and renders exactly as before.
                        items(
                            items = uiState.rows,
                            key = { it.key },
                        ) { row ->
                            when (row) {
                                is HostListRow.HostRow -> HostListItem(
                                    host = row.host,
                                    connectionState = uiState.connectionStates[row.host.id] ?: ConnectionState.UNKNOWN,
                                    onClick = {
                                        when {
                                            makingShortcut -> onSelectShortcut(row.host)
                                            row.isTabAtelier -> onToggleTabHost(row.host.id)
                                            else -> onNavigateToConsole(row.host, null)
                                        }
                                    },
                                    onEdit = { onNavigateToEditHost(row.host) },
                                    onPortForwards = { onNavigateToPortForwards(row.host) },
                                    onDuplicate = { onDuplicateHost(row.host) },
                                    onForgetHostKeys = { onForgetHostKeys(row.host) },
                                    onDisconnect = { onDisconnectHost(row.host) },
                                    onDelete = { onDeleteHost(row.host) },
                                    onRefreshTabs = { onRefreshTabs(row.host) },
                                    makingShortcut = makingShortcut,
                                    expanded = row.expanded,
                                    tabsLoading = row.tabsLoading,
                                    // The chevron does what tapping the row does
                                    // for a tab-atelier server. The child consumes
                                    // the tap, so it toggles once, not twice.
                                    onToggleExpanded = { onToggleTabHost(row.host.id) },
                                )

                                // The tab the user tapped is the session to open.
                                is HostListRow.TabRow -> TabAtelierTabRow(
                                    tab = row.tab,
                                    onOpen = { onNavigateToConsole(row.host, row.tab.id) },
                                    modifier = Modifier.testTag(
                                        HostListTestTags.tabRow(row.hostId, row.tab.id),
                                    ),
                                )

                                is HostListRow.TabStatusRow -> TabAtelierStatusRow(
                                    hostId = row.hostId,
                                    status = row.status,
                                    detail = row.detail,
                                )
                            }
                        }
                    }
                }
            }
        }
    }

    if (showDisconnectAllDialog) {
        DisconnectAllDialog(
            onDismiss = { showDisconnectAllDialog = false },
            onConfirm = {
                showDisconnectAllDialog = false
                onDisconnectAll()
            },
        )
    }
}

@Composable
private fun HostListItem(
    host: Host,
    connectionState: ConnectionState,
    onClick: () -> Unit,
    onEdit: () -> Unit,
    onPortForwards: () -> Unit,
    onDuplicate: () -> Unit,
    onForgetHostKeys: () -> Unit,
    onDisconnect: () -> Unit,
    onDelete: () -> Unit,
    modifier: Modifier = Modifier,
    makingShortcut: Boolean = false,
    // Changed for Tab Atelier Remote (Apache-2.0 section 4(b)): a tab-atelier
    // server's row carries the state of its tabs and the action to reload them.
    onRefreshTabs: () -> Unit = {},
    expanded: Boolean = false,
    tabsLoading: Boolean = false,
    onToggleExpanded: () -> Unit = {},
) {
    val isTabAtelier = host.protocol == TabAtelier.PROTOCOL
    var showMenu by remember { mutableStateOf(false) }
    var showDeleteDialog by remember { mutableStateOf(false) }
    var showDisconnectDialog by remember { mutableStateOf(false) }
    var showForgetHostKeysDialog by remember { mutableStateOf(false) }

    // Determine border color based on connection state
    val borderColor = when (connectionState) {
        ConnectionState.CONNECTED -> colorResource(R.color.host_green)

        // Green
        ConnectionState.DISCONNECTED -> colorResource(R.color.host_red)

        // Red
        ConnectionState.UNKNOWN -> Color.Transparent
    }

    Column(modifier = modifier) {
        ListItem(
            supportingContent = {
                // Changed for Tab Atelier Remote (Apache-2.0 section 4(b)): a
                // tab-atelier host's address is its URL, scheme and path prefix
                // included, so the row shows what was entered rather than the
                // internal protocol token.
                val subtitle = if (isTabAtelier) {
                    host.tabAtelierUrl ?: host.hostname
                } else {
                    "${host.protocol}://${host.hostname}:${host.port}"
                }
                Text(subtitle)
            },
            leadingContent = {
                Box(
                    modifier = Modifier.size(40.dp),
                ) {
                    // Main host icon with colored background and border
                    Box(
                        modifier = Modifier
                            .size(40.dp)
                            .background(
                                color = parseColor(host.color),
                                shape = CircleShape,
                            )
                            .border(
                                width = 3.dp,
                                color = borderColor,
                                shape = CircleShape,
                            ),
                        contentAlignment = Alignment.Center,
                    ) {
                        Icon(
                            // Changed for Tab Atelier Remote (Apache-2.0 section
                            // 4(b)): the tab-atelier type gets a terminal glyph.
                            imageVector = when (host.protocol) {
                                "ssh" -> Icons.Default.Computer
                                "telnet" -> Icons.Default.Computer
                                TabAtelier.PROTOCOL -> Icons.Default.Terminal
                                else -> Icons.Default.Link
                            },
                            contentDescription = when (connectionState) {
                                ConnectionState.CONNECTED -> stringResource(R.string.image_description_connected)
                                ConnectionState.DISCONNECTED -> stringResource(R.string.image_description_disconnected)
                                ConnectionState.UNKNOWN -> null
                            },
                            tint = Color.White,
                            modifier = Modifier.size(24.dp),
                        )
                    }

                    // Status badge icon in lower right corner
                    if (connectionState != ConnectionState.UNKNOWN) {
                        Box(
                            modifier = Modifier
                                .align(Alignment.BottomEnd)
                                .size(16.dp)
                                .background(
                                    color = MaterialTheme.colorScheme.surface,
                                    shape = CircleShape,
                                ),
                        ) {
                            Icon(
                                imageVector = when (connectionState) {
                                    ConnectionState.CONNECTED -> Icons.Default.CheckCircle
                                    ConnectionState.DISCONNECTED -> Icons.Default.Error
                                    ConnectionState.UNKNOWN -> Icons.Default.Computer // Unreachable
                                },
                                contentDescription = null,
                                tint = when (connectionState) {
                                    ConnectionState.CONNECTED -> colorResource(R.color.host_green)
                                    ConnectionState.DISCONNECTED -> colorResource(R.color.host_red)
                                    ConnectionState.UNKNOWN -> Color.Gray // Unreachable
                                },
                                modifier = Modifier.size(16.dp),
                            )
                        }
                    }
                }
            },
            trailingContent = {
                if (!makingShortcut) {
                    // Changed for Tab Atelier Remote (Apache-2.0 section 4(b)):
                    // a row's trailing content is laid out in a stack, not in
                    // sequence, so two controls put there occupy the same
                    // coordinates and whichever is drawn last takes every tap.
                    // Measured before this Row existed: the chevron and the
                    // overflow button were both at Rect.fromLTRB(1856.0, 192.0,
                    // 1936.0, 272.0), and a tap on the chevron reached the
                    // overflow button, so the tab list could not be hidden by its
                    // own control. A Row is what gives each its own space.
                    Box {
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            if (isTabAtelier) {
                                IconButton(
                                    onClick = onToggleExpanded,
                                    modifier = Modifier.testTag(HostListTestTags.itemExpandButton(host.id)),
                                ) {
                                    Icon(
                                        imageVector = if (expanded) Icons.Default.ExpandLess else Icons.Default.ExpandMore,
                                        contentDescription = stringResource(
                                            if (expanded) R.string.button_collapse else R.string.expand,
                                        ),
                                    )
                                }
                            }
                            IconButton(
                                onClick = { showMenu = true },
                                modifier = Modifier.testTag(HostListTestTags.itemMenuButton(host.id)),
                            ) {
                                Icon(Icons.Default.MoreVert, contentDescription = stringResource(R.string.button_host_options))
                            }
                        }
                        DropdownMenu(
                            expanded = showMenu,
                            onDismissRequest = { showMenu = false },
                        ) {
                            // Changed for Tab Atelier Remote (Apache-2.0 section
                            // 4(b)): re-read this server's tab list.
                            if (isTabAtelier) {
                                DropdownMenuItem(
                                    text = { Text(stringResource(R.string.tabatelier_refresh_tabs)) },
                                    onClick = {
                                        showMenu = false
                                        onRefreshTabs()
                                    },
                                    enabled = !tabsLoading,
                                    leadingIcon = {
                                        Icon(Icons.Default.Refresh, null)
                                    },
                                )
                            }
                            DropdownMenuItem(
                                text = { Text(stringResource(R.string.list_host_edit)) },
                                onClick = {
                                    showMenu = false
                                    onEdit()
                                },
                                leadingIcon = {
                                    Icon(Icons.Default.Edit, null)
                                },
                            )
                            // Changed for Tab Atelier Remote (Apache-2.0 section
                            // 4(b)): only where port forwarding can actually work.
                            //
                            // It could not, for a tab-atelier host: this app's
                            // transport reports canForwardPorts() as false and every
                            // mutator as a no-op, so the item led to a screen with a
                            // working "+" that silently forwarded nothing. The guard
                            // matches the one the console already applies
                            // (canForwardPorts), which is true for SSH alone, and it
                            // matches the sibling item below, which has always been
                            // gated this way for the same reason.
                            if (host.protocol == "ssh") {
                                DropdownMenuItem(
                                    text = { Text(stringResource(R.string.list_host_portforwards)) },
                                    onClick = {
                                        showMenu = false
                                        onPortForwards()
                                    },
                                    leadingIcon = {
                                        Icon(Icons.Default.Link, null)
                                    },
                                )
                            }
                            DropdownMenuItem(
                                text = { Text(stringResource(R.string.list_host_duplicate)) },
                                onClick = {
                                    showMenu = false
                                    onDuplicate()
                                },
                                leadingIcon = {
                                    Icon(Icons.Default.ContentCopy, null)
                                },
                            )
                            if (host.protocol == "ssh") {
                                DropdownMenuItem(
                                    text = { Text(stringResource(R.string.list_host_forget_keys)) },
                                    onClick = {
                                        showMenu = false
                                        showForgetHostKeysDialog = true
                                    },
                                    leadingIcon = {
                                        Icon(Icons.Default.Key, null)
                                    },
                                )
                            }
                            DropdownMenuItem(
                                text = { Text(stringResource(R.string.list_host_disconnect)) },
                                onClick = {
                                    showMenu = false
                                    showDisconnectDialog = true
                                },
                                enabled = connectionState == ConnectionState.CONNECTED,
                                leadingIcon = {
                                    Icon(Icons.Default.LinkOff, null)
                                },
                            )
                            DropdownMenuItem(
                                text = { Text(stringResource(R.string.list_host_delete)) },
                                onClick = {
                                    showMenu = false
                                    showDeleteDialog = true
                                },
                                leadingIcon = {
                                    Icon(Icons.Default.Delete, null)
                                },
                            )
                        }
                    }
                }
            },
            modifier = Modifier
                .clickable(onClick = onClick)
                .testTag(HostListTestTags.itemRow(host.id)),
        ) {
            Text(
                text = host.nickname,
                fontWeight = FontWeight.Bold,
            )
        }
        HorizontalDivider()

        if (showDeleteDialog) {
            HostDeleteDialog(
                host = host,
                onDismiss = { showDeleteDialog = false },
                onConfirm = {
                    showDeleteDialog = false
                    onDelete()
                },
            )
        }

        if (showDisconnectDialog) {
            HostDisconnectDialog(
                host = host,
                onDismiss = { showDisconnectDialog = false },
                onConfirm = {
                    showDisconnectDialog = false
                    onDisconnect()
                },
            )
        }

        if (showForgetHostKeysDialog) {
            ForgetHostKeysDialog(
                host = host,
                onDismiss = { showForgetHostKeysDialog = false },
                onConfirm = {
                    showForgetHostKeysDialog = false
                    onForgetHostKeys()
                },
            )
        }
    }
}

@Composable
private fun HostDeleteDialog(
    host: Host,
    onDismiss: () -> Unit,
    onConfirm: () -> Unit,
) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(stringResource(R.string.list_host_delete)) },
        text = {
            Text(stringResource(R.string.delete_host_confirm, host.nickname))
        },
        confirmButton = {
            TextButton(
                onClick = onConfirm,
            ) {
                Text(stringResource(R.string.button_yes))
            }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) {
                Text(stringResource(R.string.button_no))
            }
        },
    )
}

@Composable
private fun HostDisconnectDialog(
    host: Host,
    onDismiss: () -> Unit,
    onConfirm: () -> Unit,
) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(stringResource(R.string.list_host_disconnect)) },
        text = {
            Text(stringResource(R.string.disconnect_host_alert, host.nickname))
        },
        confirmButton = {
            TextButton(
                onClick = onConfirm,
            ) {
                Text(stringResource(R.string.button_yes))
            }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) {
                Text(stringResource(R.string.button_no))
            }
        },
    )
}

@Composable
private fun ForgetHostKeysDialog(
    host: Host,
    onDismiss: () -> Unit,
    onConfirm: () -> Unit,
) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(stringResource(R.string.list_host_forget_keys)) },
        text = {
            Text(stringResource(R.string.forget_host_keys_confirm, host.nickname))
        },
        confirmButton = {
            TextButton(
                onClick = onConfirm,
            ) {
                Text(stringResource(R.string.button_yes))
            }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) {
                Text(stringResource(R.string.button_no))
            }
        },
    )
}

@Composable
private fun parseColor(colorString: String?): Color {
    if (colorString.isNullOrBlank()) {
        return colorResource(R.color.host_blue)
    } else {
        val colorInt = colorString.toColorInt()
        return Color(colorInt)
    }
}

@PreviewScreen
@Composable
private fun HostListScreenEmptyPreview() {
    ConnectBotTheme {
        HostListScreenContent(
            uiState = HostListUiState(
                hosts = emptyList(),
                isLoading = false,
            ),
            onNavigateToConsole = { _, _ -> },
            onNavigateToEditHost = {},
            onNavigateToSettings = {},
            onNavigateToPubkeys = {},
            onNavigateToPortForwards = {},
            onNavigateToProfiles = {},
            onNavigateToHelp = {},
            onToggleSortOrder = {},
            onDeleteHost = {},
            onDuplicateHost = {},
            onForgetHostKeys = {},
            onDisconnectHost = {},
            onDisconnectAll = {},
        )
    }
}

@PreviewScreen
@Composable
private fun HostListScreenLoadingPreview() {
    ConnectBotTheme {
        HostListScreenContent(
            uiState = HostListUiState(
                hosts = emptyList(),
                isLoading = true,
            ),
            onNavigateToConsole = { _, _ -> },
            onNavigateToEditHost = {},
            onNavigateToSettings = {},
            onNavigateToPubkeys = {},
            onNavigateToPortForwards = {},
            onNavigateToProfiles = {},
            onNavigateToHelp = {},
            onToggleSortOrder = {},
            onDeleteHost = {},
            onDuplicateHost = {},
            onForgetHostKeys = {},
            onDisconnectHost = {},
            onDisconnectAll = {},
        )
    }
}

@PreviewScreen
@Composable
private fun HostListScreenErrorPreview() {
    ConnectBotTheme {
        HostListScreenContent(
            uiState = HostListUiState(
                hosts = emptyList(),
                isLoading = false,
                error = "Failed to load hosts from database",
            ),
            onNavigateToConsole = { _, _ -> },
            onNavigateToEditHost = {},
            onNavigateToSettings = {},
            onNavigateToPubkeys = {},
            onNavigateToPortForwards = {},
            onNavigateToProfiles = {},
            onNavigateToHelp = {},
            onToggleSortOrder = {},
            onDeleteHost = {},
            onDuplicateHost = {},
            onForgetHostKeys = {},
            onDisconnectHost = {},
            onDisconnectAll = {},
        )
    }
}

@PreviewScreen
@Composable
private fun HostListScreenPopulatedPreview() {
    ConnectBotTheme {
        HostListScreenContent(
            uiState = HostListUiState(
                hosts = listOf(
                    Host(
                        id = 1,
                        nickname = "Production Server",
                        protocol = "ssh",
                        username = "root",
                        hostname = "prod.example.com",
                        port = 22,
                        color = "#4CAF50",
                    ),
                    Host(
                        id = 2,
                        nickname = "Development",
                        protocol = "ssh",
                        username = "developer",
                        hostname = "dev.example.com",
                        port = 2222,
                        color = "#2196F3",
                    ),
                    Host(
                        id = 3,
                        nickname = "Local VM",
                        protocol = "ssh",
                        username = "admin",
                        hostname = "192.168.1.100",
                        port = 22,
                        color = "#FF9800",
                    ),
                ),
                connectionStates = mapOf(
                    1L to ConnectionState.CONNECTED,
                    2L to ConnectionState.DISCONNECTED,
                    3L to ConnectionState.UNKNOWN,
                ),
                isLoading = false,
            ),
            onNavigateToConsole = { _, _ -> },
            onNavigateToEditHost = {},
            onNavigateToSettings = {},
            onNavigateToPubkeys = {},
            onNavigateToPortForwards = {},
            onNavigateToProfiles = {},
            onNavigateToHelp = {},
            onToggleSortOrder = {},
            onDeleteHost = {},
            onDuplicateHost = {},
            onForgetHostKeys = {},
            onDisconnectHost = {},
            onDisconnectAll = {},
        )
    }
}

@Composable
private fun StartupKeyPasswordDialog(
    pubkey: Pubkey,
    wrongPassword: Boolean,
    onDismiss: () -> Unit,
    onProvidePassword: (String) -> Unit,
) {
    var password by remember(pubkey.id) { mutableStateOf("") }

    TextInputAlertDialog(
        onDismissRequest = onDismiss,
        onConfirm = { onProvidePassword(password) },
        value = password,
        onValueChange = { password = it },
        confirmButtonText = stringResource(R.string.pubkey_unlock),
        icon = { Icon(Icons.Default.Lock, contentDescription = null) },
        title = { Text(stringResource(R.string.pubkey_unlock)) },
        message = {
            Text(
                text = stringResource(R.string.pubkey_unlock_message, pubkey.nickname),
                modifier = Modifier.padding(bottom = 16.dp),
            )
        },
        label = { Text(stringResource(R.string.prompt_password)) },
        supportingText = if (wrongPassword) {
            { Text(stringResource(R.string.alert_wrong_password_msg)) }
        } else {
            null
        },
        isError = wrongPassword,
        isPassword = true,
    )
}

/**
 * One tab of a tab-atelier daemon, nested under its server's row.
 *
 * The row shows the tab's name with its last output line beneath it, plus
 * unobtrusive markers for the states the daemon reports. Tapping it does
 * nothing yet.
 *
 * Changed for Tab Atelier Remote (Apache-2.0 section 4(b)): new, upstream has
 * no nested rows.
 */
@Composable
private fun TabAtelierTabRow(
    tab: TabAtelierTab,
    onOpen: () -> Unit,
    modifier: Modifier = Modifier,
) {
    ListItem(
        leadingContent = {
            Icon(
                imageVector = Icons.Default.Terminal,
                contentDescription = null,
                tint = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.size(20.dp),
            )
        },
        supportingContent = {
            tab.preview?.let { preview ->
                Text(
                    text = preview,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        },
        trailingContent = {
            Row(verticalAlignment = Alignment.CenterVertically) {
                tab.agentState?.let { agentState ->
                    TabMarker(text = agentState)
                }
                if (tab.active) {
                    TabMarker(
                        text = stringResource(R.string.tabatelier_tab_active),
                        color = colorResource(R.color.host_green),
                    )
                }
                if (tab.locked) {
                    Icon(
                        imageVector = Icons.Default.Lock,
                        contentDescription = stringResource(R.string.tabatelier_tab_locked),
                        tint = MaterialTheme.colorScheme.onSurfaceVariant,
                        modifier = Modifier.size(16.dp),
                    )
                }
            }
        },
        colors = ListItemDefaults.colors(containerColor = Color.Transparent),
        modifier = modifier
            .fillMaxWidth()
            .padding(start = TAB_INDENT)
            // Opens this tab's session. The host row holds the daemon; this row
            // holds which of its terminals to attach to.
            .clickable(onClick = onOpen),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text(
                text = tab.name.ifEmpty { tab.id },
                fontWeight = FontWeight.Medium,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.weight(1f, fill = false),
            )
            tab.badge?.let { badge ->
                Spacer(modifier = Modifier.width(8.dp))
                Text(
                    text = badge,
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
    }
}

/**
 * A short, muted marker on a tab row — an agent state, or "active".
 */
@Composable
private fun TabMarker(
    text: String,
    color: Color = MaterialTheme.colorScheme.onSurfaceVariant,
) {
    Text(
        text = text,
        style = MaterialTheme.typography.labelSmall,
        color = color,
        maxLines = 1,
        overflow = TextOverflow.Ellipsis,
        modifier = Modifier.padding(start = 8.dp),
    )
}

/**
 * The note under a tab-atelier server's row when it has no tabs to show: it is
 * loading, it has none, or its fetch failed. A failed server shows its own note
 * and does not affect any other row on the list.
 *
 * Changed for Tab Atelier Remote (Apache-2.0 section 4(b)): new.
 */
@Composable
private fun TabAtelierStatusRow(
    hostId: Long,
    status: TabStatus,
    detail: String?,
    modifier: Modifier = Modifier,
) {
    val text = when (status) {
        TabStatus.LOADING -> stringResource(R.string.tabatelier_tabs_loading)
        TabStatus.EMPTY -> stringResource(R.string.tabatelier_tabs_empty)
        TabStatus.ERROR -> stringResource(R.string.tabatelier_tabs_error, detail.orEmpty())
    }

    Row(
        modifier = modifier
            .fillMaxWidth()
            .padding(start = TAB_INDENT, end = 16.dp, top = 4.dp, bottom = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        if (status == TabStatus.LOADING) {
            CircularProgressIndicator(
                modifier = Modifier.size(16.dp),
                strokeWidth = 2.dp,
            )
            Spacer(modifier = Modifier.width(8.dp))
        }
        Text(
            text = text,
            style = MaterialTheme.typography.bodySmall,
            color = if (status == TabStatus.ERROR) {
                MaterialTheme.colorScheme.error
            } else {
                MaterialTheme.colorScheme.onSurfaceVariant
            },
            modifier = Modifier.testTag(HostListTestTags.tabStatus(hostId)),
        )
    }
}

/** How far a tab row is indented under its server's row. */
private val TAB_INDENT = 44.dp
