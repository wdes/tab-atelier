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

import android.content.Context
import android.content.SharedPreferences
import androidx.core.content.edit
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import dagger.hilt.android.lifecycle.HiltViewModel
import dagger.hilt.android.qualifiers.ApplicationContext
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.flatMapLatest
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.connectbot.R
import org.connectbot.data.HostRepository
import org.connectbot.data.entity.Host
import org.connectbot.data.entity.Pubkey
import org.connectbot.di.CoroutineDispatchers
import org.connectbot.service.ServiceError
import org.connectbot.service.TerminalManager
import org.connectbot.tabatelier.TabAtelierClient
import org.connectbot.tabatelier.TabAtelierTab
import org.connectbot.tabatelier.tabAtelierErrorMessage
import org.connectbot.transport.TabAtelier
import org.connectbot.util.PreferenceConstants
import javax.inject.Inject

enum class ConnectionState {
    UNKNOWN,
    CONNECTED,
    DISCONNECTED,
}

/**
 * One row of the host list.
 *
 * A tab-atelier host expands into its daemon's tabs, so the list is a single
 * flattened row list rather than a list of hosts: `LazyColumn` keys must be
 * unique across hosts and tabs alike.
 *
 * New type for Tab Atelier Remote (Apache-2.0 section 4(b)): upstream renders
 * `uiState.hosts` directly.
 */
sealed class HostListRow {
    /** Stable key for `LazyColumn`, unique across hosts and tabs. */
    abstract val key: String

    /** A host, as upstream renders it. */
    data class HostRow(
        val host: Host,
        val expanded: Boolean = false,
        val tabsLoading: Boolean = false,
    ) : HostListRow() {
        override val key: String = "host-${host.id}"

        val isTabAtelier: Boolean get() = host.protocol == TabAtelier.PROTOCOL
    }

    /** One tab of a tab-atelier host, indented under its server row. */
    data class TabRow(
        // Changed for Tab Atelier Remote (Apache-2.0 section 4(b)): the tab's
        // host, so tapping the row can open that host's session. hostId alone
        // would mean looking the host up again in the UI layer.
        val host: Host,
        val hostId: Long,
        val index: Int,
        val tab: TabAtelierTab,
    ) : HostListRow() {
        // The daemon may omit a tab id; the index keeps keys unique either way.
        override val key: String = "host-$hostId-tab-${tab.id.ifEmpty { "index-$index" }}"
    }

    /** A note under a tab-atelier host's row: loading, empty, or a failure. */
    data class TabStatusRow(
        val hostId: Long,
        val status: TabStatus,
        val detail: String? = null,
    ) : HostListRow() {
        override val key: String = "host-$hostId-tab-status"
    }
}

/** Why a tab-atelier host's row has no tabs under it. */
enum class TabStatus {
    LOADING,
    EMPTY,
    ERROR,
}

/**
 * What the list knows about one tab-atelier host's tabs.
 *
 * Held per host id, so one unreachable server shows its own error and nothing
 * else on the list is affected.
 */
data class TabListState(
    val loading: Boolean = false,
    val tabs: List<TabAtelierTab> = emptyList(),
    val error: String? = null,
)

data class HostListUiState(
    val hosts: List<Host> = emptyList(),
    // Changed for Tab Atelier Remote (Apache-2.0 section 4(b)): the per-host tab
    // state and which tab-atelier servers are collapsed.
    val tabStates: Map<Long, TabListState> = emptyMap(),
    val collapsedTabHosts: Set<Long> = emptySet(),
    val connectionStates: Map<Long, ConnectionState> = emptyMap(),
    val isLoading: Boolean = false,
    val error: String? = null,
    val sortedByColor: Boolean = false,
    val exportedJson: String? = null,
    val exportResult: ExportResult? = null,
    val importResult: ImportResult? = null,
    val startupKeyPrompt: Pubkey? = null,
    val startupKeyWrongPassword: Boolean = false,
) {
    /**
     * The host list flattened into rows: hosts, and under an expanded
     * tab-atelier host its daemon's tabs.
     *
     * Derived rather than stored, so a state built by hand — a preview, a test —
     * still renders its hosts, and there is no second copy of the list to keep
     * in step. `LazyColumn` keys must be unique across hosts and tabs alike,
     * which is why the two live in one row list.
     */
    val rows: List<HostListRow>
        get() = flattenHostRows(hosts, tabStates, collapsedTabHosts)
}

/**
 * Flatten hosts and their tabs into one row list.
 *
 * A host that is not a tab-atelier server contributes exactly its own row, so
 * nothing about its appearance or behaviour changes.
 */
private fun flattenHostRows(
    hosts: List<Host>,
    tabStates: Map<Long, TabListState>,
    collapsed: Set<Long>,
): List<HostListRow> = buildList {
    hosts.forEach { host ->
        val expanded = host.protocol == TabAtelier.PROTOCOL && host.id !in collapsed
        val state = tabStates[host.id]
        add(
            HostListRow.HostRow(
                host = host,
                expanded = expanded,
                tabsLoading = state?.loading == true,
            ),
        )
        if (!expanded) return@forEach

        when {
            // A refresh that already has tabs to show keeps showing them; the
            // loading note is only for the first fetch.
            state == null || (state.loading && state.tabs.isEmpty()) -> add(
                HostListRow.TabStatusRow(host.id, TabStatus.LOADING),
            )

            state.error != null -> add(
                HostListRow.TabStatusRow(host.id, TabStatus.ERROR, state.error),
            )

            state.tabs.isEmpty() -> add(
                HostListRow.TabStatusRow(host.id, TabStatus.EMPTY),
            )
        }
        state?.tabs?.forEachIndexed { index, tab ->
            add(HostListRow.TabRow(host, host.id, index, tab))
        }
    }
}

data class ImportResult(
    val hostsImported: Int,
    val hostsSkipped: Int,
    val profilesImported: Int,
    val profilesSkipped: Int,
)

data class ExportResult(
    val hostCount: Int,
    val profileCount: Int,
)

@HiltViewModel
class HostListViewModel @Inject constructor(
    @param:ApplicationContext private val context: Context,
    private val repository: HostRepository,
    private val dispatchers: CoroutineDispatchers,
    private val sharedPreferences: SharedPreferences,
    private val tabAtelierClient: TabAtelierClient,
) : ViewModel() {

    private var terminalManager: TerminalManager? = null

    /**
     * What a tab-atelier host's probe depends on: the URL it is reached at, and
     * whether a token is stored for it. Nothing else about a host is part of it,
     * and `lastConnect` in particular is not: ConnectBot touches it on every
     * connect and the hosts Flow emits for that, so re-probing on every emission
     * would hit the daemon each time a session starts.
     */
    private data class ProbeFingerprint(val url: String, val hasToken: Boolean)

    /**
     * The fingerprint each tab-atelier host was last probed with, by host id.
     * A host is probed when it is new, or when this no longer matches — which is
     * what an edit that changes its URL or its token does, and nothing else.
     */
    private val probeFingerprints = mutableMapOf<Long, ProbeFingerprint>()
    private val _uiState = MutableStateFlow(
        HostListUiState(
            isLoading = true,
            sortedByColor = sharedPreferences.getBoolean(PreferenceConstants.SORT_BY_COLOR, false),
        ),
    )
    val uiState: StateFlow<HostListUiState> = _uiState.asStateFlow()

    init {
        observeHosts()
    }

    fun setTerminalManager(manager: TerminalManager) {
        if (terminalManager != manager) {
            terminalManager = manager
            // Observe host status changes from Flow
            observeHostStatusChanges()
            // Collect service errors from TerminalManager
            collectServiceErrors()
            // Surface any encrypted keys that are waiting for a passphrase to be entered
            observePendingStartupKeyPrompts()
            // Update initial connection states
            updateConnectionStates(_uiState.value.hosts)
        }
    }

    @OptIn(kotlinx.coroutines.ExperimentalCoroutinesApi::class)
    private fun observeHosts() {
        viewModelScope.launch {
            _uiState
                .map { it.sortedByColor }
                .distinctUntilChanged()
                .flatMapLatest { sortedByColor ->
                    if (sortedByColor) {
                        repository.observeHostsSortedByColor()
                    } else {
                        repository.observeHosts()
                    }
                }
                .collect { hosts ->
                    updateConnectionStates(hosts)
                    _uiState.update {
                        it.copy(hosts = hosts, isLoading = false, error = null)
                    }
                    // Changed for Tab Atelier Remote (Apache-2.0 section 4(b)):
                    // fetch each tab-atelier server's tabs as the list loads.
                    syncTabAtelierHosts(hosts)
                }
        }
    }

    /**
     * Probe every tab-atelier host whose [ProbeFingerprint] has changed, and
     * drop state for hosts that are gone.
     */
    private fun syncTabAtelierHosts(hosts: List<Host>) {
        val tabHosts = hosts.filter { it.protocol == TabAtelier.PROTOCOL }
        val ids = tabHosts.map { it.id }.toSet()
        probeFingerprints.keys.retainAll(ids)
        _uiState.update { state ->
            state.copy(tabStates = state.tabStates.filterKeys { it in ids })
        }
        tabHosts.forEach { host ->
            val fingerprint = fingerprintOf(host)
            if (probeFingerprints[host.id] == fingerprint) return@forEach
            probeFingerprints[host.id] = fingerprint
            // An edited server is probed from scratch: what the last probe found
            // belongs to an address or a token that is no longer this server's,
            // and leaving its error on the row is what makes a server someone
            // just fixed look broken until something else reloads the list.
            forgetTabs(host.id)
            fetchTabs(host)
        }
    }

    /** What a host's probe depends on: its URL, and whether it has a token. */
    private fun fingerprintOf(host: Host): ProbeFingerprint = ProbeFingerprint(
        url = host.tabAtelierUrl.orEmpty(),
        hasToken = tabAtelierClient.hasToken(host.id),
    )

    /** Drop a host's tabs, and with them whatever the last probe said. */
    private fun forgetTabs(hostId: Long) {
        _uiState.update { state -> state.copy(tabStates = state.tabStates - hostId) }
    }

    /**
     * Reload one tab-atelier host's tabs. Also the "offer a refresh" action on
     * its row.
     */
    fun refreshTabs(host: Host) {
        if (host.protocol != TabAtelier.PROTOCOL) return
        // A manual refresh probes even when nothing about the server changed,
        // and records what it probed with, so the next unchanged emission does
        // not read as a change and probe a second time.
        probeFingerprints[host.id] = fingerprintOf(host)
        fetchTabs(host)
    }

    /**
     * Refresh every tab-atelier server's tabs at once, for the pull-to-refresh gesture.
     *
     * Per server rather than per row, because the gesture is made on the list and means
     * "all of this may be stale" — and because forcing a probe is what makes it useful
     * against a server whose tabs look stuck: [refreshTabs] probes even when the
     * fingerprint says nothing about the server changed, which is exactly the case the
     * automatic probe skips.
     *
     * A server that is not a tab-atelier one has nothing to fetch, so it is left alone
     * rather than given a failure it cannot act on.
     */
    fun refreshAllTabAtelierTabs() {
        _uiState.value.hosts
            .filter { it.protocol == TabAtelier.PROTOCOL }
            .forEach(::refreshTabs)
    }

    /**
     * Expand or collapse a tab-atelier host's tabs.
     */
    fun toggleTabHost(hostId: Long) {
        _uiState.update { state ->
            val collapsed = if (hostId in state.collapsedTabHosts) {
                state.collapsedTabHosts - hostId
            } else {
                state.collapsedTabHosts + hostId
            }
            state.copy(collapsedTabHosts = collapsed)
        }
    }

    private fun fetchTabs(host: Host) {
        viewModelScope.launch {
            _uiState.update { state ->
                val previous = state.tabStates[host.id]
                val tabStates = state.tabStates +
                    (host.id to TabListState(loading = true, tabs = previous?.tabs ?: emptyList()))
                state.copy(tabStates = tabStates)
            }

            val result = try {
                Result.success(
                    withContext(dispatchers.io) {
                        // Changed for Tab Atelier Remote (Apache-2.0 section
                        // 4(b)): the host is addressed by its URL, which carries
                        // the scheme — http is not upgraded to https — and any
                        // path prefix. A URL that does not parse is reported as
                        // this server's error rather than fetched from a guess.
                        val base = tabAtelierClient.base(host.tabAtelierUrl)
                            ?: throw IllegalStateException(
                                context.getString(R.string.tabatelier_url_invalid),
                            )
                        tabAtelierClient.fetchTabs(host.id, base, tabAtelierClient.token(host.id))
                    },
                )
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                Result.failure(e)
            }

            _uiState.update { state ->
                val previous = state.tabStates[host.id]
                val tabState = result.fold(
                    onSuccess = { TabListState(loading = false, tabs = it) },
                    onFailure = {
                        TabListState(
                            loading = false,
                            tabs = previous?.tabs ?: emptyList(),
                            error = tabAtelierErrorMessage(it),
                        )
                    },
                )
                state.copy(tabStates = state.tabStates + (host.id to tabState))
            }
        }
    }

    private fun observeHostStatusChanges() {
        val manager = terminalManager ?: return
        viewModelScope.launch {
            manager.hostStatusChangedFlow.collect {
                // Update connection states when terminal manager notifies us of changes
                updateConnectionStates(_uiState.value.hosts)
            }
        }
    }

    private fun collectServiceErrors() {
        val manager = terminalManager ?: return
        viewModelScope.launch {
            manager.serviceErrors.collect { error ->
                val errorMessage = formatServiceError(error)
                _uiState.update { it.copy(error = errorMessage) }
            }
        }
    }

    private fun formatServiceError(error: ServiceError): String = when (error) {
        is ServiceError.KeyLoadFailed -> {
            context.getString(R.string.error_key_load_failed, error.keyName, error.reason)
        }

        is ServiceError.ConnectionFailed -> {
            context.getString(
                R.string.error_connection_failed,
                error.hostNickname,
                error.hostname,
                error.reason,
            )
        }

        is ServiceError.PortForwardLoadFailed -> {
            context.getString(
                R.string.error_port_forward_load_failed,
                error.hostNickname,
                error.reason,
            )
        }

        is ServiceError.HostSaveFailed -> {
            context.getString(R.string.error_host_save_failed, error.hostNickname, error.reason)
        }

        is ServiceError.ColorSchemeLoadFailed -> {
            context.getString(R.string.error_color_scheme_load_failed, error.reason)
        }
    }

    private fun updateConnectionStates(hosts: List<Host>) {
        val states = hosts.associate { host ->
            host.id to getConnectionState(host)
        }
        _uiState.update { it.copy(connectionStates = states) }
    }

    private fun getConnectionState(host: Host): ConnectionState {
        val manager = terminalManager ?: return ConnectionState.UNKNOWN

        // Check if host has an active bridge
        val bridge = manager.bridgesFlow.value.find { it.host.id == host.id }
        if (bridge != null) {
            // Bridge exists but may be disconnected or in grace period
            return if (bridge.disconnected || bridge.isInGracePeriod()) {
                ConnectionState.DISCONNECTED
            } else {
                ConnectionState.CONNECTED
            }
        }

        // Check if in disconnected list by comparing ID
        if (manager.disconnectedFlow.value.any { it.id == host.id }) {
            return ConnectionState.DISCONNECTED
        }

        return ConnectionState.UNKNOWN
    }

    fun toggleSortOrder() {
        val newSortedByColor = !_uiState.value.sortedByColor
        sharedPreferences.edit { putBoolean(PreferenceConstants.SORT_BY_COLOR, newSortedByColor) }
        _uiState.update { it.copy(sortedByColor = newSortedByColor) }
    }

    fun deleteHost(host: Host) {
        viewModelScope.launch {
            try {
                terminalManager?.disconnectHost(host.id)
                repository.deleteHost(host)
                // Changed for Tab Atelier Remote (Apache-2.0 section 4(b)):
                // the host's API token and certificate pin go with it, the way
                // the repository drops an SSH password.
                if (host.protocol == TabAtelier.PROTOCOL) {
                    tabAtelierClient.forgetHost(host.id)
                }
                probeFingerprints.remove(host.id)
            } catch (e: Exception) {
                _uiState.update {
                    it.copy(error = e.message ?: "Failed to delete host")
                }
            }
        }
    }

    fun duplicateHost(host: Host) {
        viewModelScope.launch {
            try {
                // Create new host with reset fields
                val newHost = host.copy(
                    id = 0L,
                    nickname = context.getString(R.string.host_duplicate_nickname, host.nickname),
                    lastConnect = 0,
                    hostKeyAlgo = null,
                )
                repository.duplicateHostSettings(host.id, newHost)
            } catch (e: Exception) {
                _uiState.update {
                    it.copy(error = e.message ?: "Failed to duplicate host")
                }
            }
        }
    }

    fun forgetHostKeys(host: Host) {
        viewModelScope.launch {
            try {
                repository.deleteKnownHostsForHost(host.id)
            } catch (e: Exception) {
                _uiState.update {
                    it.copy(error = e.message ?: "Failed to forget host keys")
                }
            }
        }
    }

    fun disconnectAll() {
        terminalManager?.disconnectAll(excludeLocal = false)
    }

    fun disconnectHost(host: Host) {
        terminalManager?.disconnectHost(host.id)
    }

    fun clearError() {
        _uiState.update { it.copy(error = null) }
    }

    fun exportHosts() {
        viewModelScope.launch {
            try {
                val (json, exportCounts) = withContext(dispatchers.io) {
                    repository.exportHostsToJson()
                }
                val exportResult = ExportResult(
                    hostCount = exportCounts.hostCount,
                    profileCount = exportCounts.profileCount,
                )
                _uiState.update { it.copy(exportedJson = json, exportResult = exportResult) }
            } catch (e: Exception) {
                _uiState.update {
                    it.copy(error = e.message ?: "Failed to export hosts")
                }
            }
        }
    }

    fun clearExportedJson() {
        _uiState.update { it.copy(exportedJson = null, exportResult = null) }
    }

    fun importHosts(jsonString: String) {
        viewModelScope.launch {
            try {
                val importCounts = withContext(dispatchers.io) {
                    repository.importHostsFromJson(jsonString)
                }
                val importResult = ImportResult(
                    hostsImported = importCounts.hostsImported,
                    hostsSkipped = importCounts.hostsSkipped,
                    profilesImported = importCounts.profilesImported,
                    profilesSkipped = importCounts.profilesSkipped,
                )
                _uiState.update { it.copy(importResult = importResult) }
            } catch (e: Exception) {
                _uiState.update {
                    it.copy(error = e.message ?: "Failed to import hosts")
                }
            }
        }
    }

    fun clearImportResult() {
        _uiState.update { it.copy(importResult = null) }
    }

    private fun observePendingStartupKeyPrompts() {
        val manager = terminalManager ?: return
        viewModelScope.launch {
            manager.pendingStartupKeyPrompts.collect { queue ->
                val head = queue.firstOrNull()
                _uiState.update { state ->
                    state.copy(
                        startupKeyPrompt = head,
                        // Reset wrong-password flag whenever the head of the queue changes
                        startupKeyWrongPassword = if (head?.id != state.startupKeyPrompt?.id) {
                            false
                        } else {
                            state.startupKeyWrongPassword
                        },
                    )
                }
            }
        }
    }

    fun submitStartupKeyPassword(password: String) {
        val manager = terminalManager ?: return
        val pubkey = _uiState.value.startupKeyPrompt ?: return
        viewModelScope.launch {
            val unlocked = withContext(dispatchers.default) {
                manager.unlockPendingStartupKey(pubkey, password)
            }
            if (!unlocked) {
                _uiState.update { it.copy(startupKeyWrongPassword = true) }
            }
        }
    }

    fun dismissStartupKeyPrompt() {
        val manager = terminalManager ?: return
        val pubkey = _uiState.value.startupKeyPrompt ?: return
        manager.dismissPendingStartupKey(pubkey)
    }
}
