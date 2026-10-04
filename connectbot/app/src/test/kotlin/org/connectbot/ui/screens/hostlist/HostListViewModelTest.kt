/*
 * ConnectBot: simple, powerful, open-source SSH client for Android
 * Copyright 2025 Kenny Root
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
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.test.UnconfinedTestDispatcher
import kotlinx.coroutines.test.advanceUntilIdle
import kotlinx.coroutines.test.resetMain
import kotlinx.coroutines.test.runTest
import kotlinx.coroutines.test.setMain
import org.connectbot.data.HostRepository
import org.connectbot.data.entity.Host
import org.connectbot.di.CoroutineDispatchers
import org.connectbot.service.ServiceError
import org.connectbot.service.TerminalManager
import org.connectbot.tabatelier.TabAtelierBase
import org.connectbot.tabatelier.TabAtelierClient
import org.connectbot.tabatelier.TabAtelierTab
import org.connectbot.transport.TabAtelier
import org.connectbot.util.PreferenceConstants
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.mockito.kotlin.any
import org.mockito.kotlin.anyOrNull
import org.mockito.kotlin.inOrder
import org.mockito.kotlin.mock
import org.mockito.kotlin.times
import org.mockito.kotlin.verify
import org.mockito.kotlin.whenever

/**
 * Tests for HostListViewModel, focusing on sort order preference persistence.
 */
@OptIn(ExperimentalCoroutinesApi::class)
class HostListViewModelTest {

    private val testDispatcher = UnconfinedTestDispatcher()
    private val dispatchers = CoroutineDispatchers(
        default = testDispatcher,
        io = testDispatcher,
        main = testDispatcher,
    )
    private lateinit var context: Context
    private lateinit var repository: HostRepository
    private lateinit var sharedPreferences: SharedPreferences
    private lateinit var editor: SharedPreferences.Editor
    private lateinit var hostsFlow: MutableStateFlow<List<Host>>
    private lateinit var hostsSortedByColorFlow: MutableStateFlow<List<Host>>
    private lateinit var tabAtelierClient: TabAtelierClient

    @Before
    fun setUp() {
        Dispatchers.setMain(testDispatcher)

        context = mock()
        repository = mock()
        sharedPreferences = mock()
        editor = mock()
        tabAtelierClient = mock()
        hostsFlow = MutableStateFlow(emptyList())
        hostsSortedByColorFlow = MutableStateFlow(emptyList())

        whenever(repository.observeHosts()).thenReturn(hostsFlow)
        whenever(repository.observeHostsSortedByColor()).thenReturn(hostsSortedByColorFlow)
        whenever(sharedPreferences.edit()).thenReturn(editor)
        whenever(editor.putBoolean(any(), any())).thenReturn(editor)
    }

    @After
    fun tearDown() {
        Dispatchers.resetMain()
    }

    private fun createViewModel(sortedByColor: Boolean = false): HostListViewModel {
        whenever(sharedPreferences.getBoolean(PreferenceConstants.SORT_BY_COLOR, false))
            .thenReturn(sortedByColor)
        return HostListViewModel(context, repository, dispatchers, sharedPreferences, tabAtelierClient)
    }

    private fun createTerminalManager(): TerminalManager {
        val terminalManager = mock<TerminalManager>()
        whenever(terminalManager.bridgesFlow).thenReturn(MutableStateFlow(emptyList()))
        whenever(terminalManager.disconnectedFlow).thenReturn(MutableStateFlow(emptyList()))
        whenever(terminalManager.hostStatusChangedFlow).thenReturn(MutableSharedFlow())
        whenever(terminalManager.serviceErrors).thenReturn(MutableSharedFlow<ServiceError>())
        whenever(terminalManager.pendingStartupKeyPrompts).thenReturn(MutableStateFlow(emptyList()))
        return terminalManager
    }

    /**
     * Tests that sort order preference is loaded from SharedPreferences on initialization.
     *
     * Scenario: User previously selected "sort by color" and restarts the app.
     * Expected: The ViewModel initializes with sortedByColor=true.
     */
    @Test
    fun init_loadsSortOrderFromPreferences_whenSortedByColorTrue() = runTest {
        val viewModel = createViewModel(sortedByColor = true)
        advanceUntilIdle()

        assertTrue("sortedByColor should be true from preferences", viewModel.uiState.value.sortedByColor)
        verify(sharedPreferences).getBoolean(PreferenceConstants.SORT_BY_COLOR, false)
    }

    /**
     * Tests that sort order defaults to false when preference is not set.
     *
     * Scenario: Fresh install or preference never set.
     * Expected: The ViewModel initializes with sortedByColor=false.
     */
    @Test
    fun init_loadsSortOrderFromPreferences_whenSortedByColorFalse() = runTest {
        val viewModel = createViewModel(sortedByColor = false)
        advanceUntilIdle()

        assertFalse("sortedByColor should be false from preferences", viewModel.uiState.value.sortedByColor)
        verify(sharedPreferences).getBoolean(PreferenceConstants.SORT_BY_COLOR, false)
    }

    /**
     * Tests that toggleSortOrder persists the new value to SharedPreferences.
     *
     * Scenario: User toggles sort order from name to color.
     * Expected: The preference is saved to SharedPreferences.
     */
    @Test
    fun toggleSortOrder_persistsPreference_whenTogglingToTrue() = runTest {
        val viewModel = createViewModel(sortedByColor = false)
        advanceUntilIdle()

        viewModel.toggleSortOrder()
        advanceUntilIdle()

        assertTrue("sortedByColor should be true after toggle", viewModel.uiState.value.sortedByColor)
        verify(editor).putBoolean(PreferenceConstants.SORT_BY_COLOR, true)
        verify(editor).apply()
    }

    /**
     * Tests that toggleSortOrder persists false when toggling back to name sort.
     *
     * Scenario: User toggles sort order from color back to name.
     * Expected: The preference is saved to SharedPreferences with false.
     */
    @Test
    fun toggleSortOrder_persistsPreference_whenTogglingToFalse() = runTest {
        val viewModel = createViewModel(sortedByColor = true)
        advanceUntilIdle()

        viewModel.toggleSortOrder()
        advanceUntilIdle()

        assertFalse("sortedByColor should be false after toggle", viewModel.uiState.value.sortedByColor)
        verify(editor).putBoolean(PreferenceConstants.SORT_BY_COLOR, false)
        verify(editor).apply()
    }

    /**
     * Tests that toggling sort order switches the data source.
     *
     * Scenario: User toggles to sort by color.
     * Expected: The ViewModel observes hosts sorted by color from the repository.
     */
    @Test
    fun toggleSortOrder_switchesToColorSortedHosts() = runTest {
        val viewModel = createViewModel(sortedByColor = false)
        advanceUntilIdle()

        viewModel.toggleSortOrder()
        advanceUntilIdle()

        verify(repository).observeHostsSortedByColor()
    }

    /**
     * Tests that the ViewModel uses alphabetical sort when sortedByColor is false.
     *
     * Scenario: Default state or user prefers alphabetical sorting.
     * Expected: The ViewModel observes hosts sorted alphabetically from the repository.
     */
    @Test
    fun init_usesAlphabeticalSort_whenSortedByColorFalse() = runTest {
        createViewModel(sortedByColor = false)
        advanceUntilIdle()

        verify(repository).observeHosts()
    }

    /**
     * Tests that the ViewModel uses color sort on init when preference is true.
     *
     * Scenario: User previously selected color sort.
     * Expected: The ViewModel observes hosts sorted by color from the repository.
     */
    @Test
    fun init_usesColorSort_whenSortedByColorTrue() = runTest {
        createViewModel(sortedByColor = true)
        advanceUntilIdle()

        verify(repository).observeHostsSortedByColor()
    }

    @Test
    fun deleteHost_disconnectsActiveBridgeBeforeDeletingHost() = runTest {
        val viewModel = createViewModel()
        val terminalManager = createTerminalManager()
        val host = Host(id = 42L, nickname = "test", hostname = "example.com")
        viewModel.setTerminalManager(terminalManager)
        advanceUntilIdle()

        viewModel.deleteHost(host)
        advanceUntilIdle()

        val inOrder = inOrder(terminalManager, repository)
        inOrder.verify(terminalManager).disconnectHost(host.id)
        inOrder.verify(repository).deleteHost(host)
    }

    // ---- What re-probes a tab-atelier server, and what deliberately does not -
    //
    // A host's tabs are keyed by host id and only re-read when the fingerprint
    // of what decides the probe — its URL, and whether a token is stored for it
    // — changes. These tests pin which host writes reach the daemon, because
    // the hosts Flow also emits for writes that have nothing to do with the
    // server's configuration. See HostListViewModel.ProbeFingerprint.

    private fun tabAtelierHost(id: Long = 7L, url: String = DAEMON_URL) = Host(
        id = id,
        nickname = "daemon",
        protocol = TabAtelier.PROTOCOL,
        hostname = "daemon.example",
        port = 443,
        tabAtelierUrl = url,
    )

    /** TabAtelierBase.parse is pure, so the URL parse under test is the real one. */
    private fun stubBase(vararg urls: String) {
        urls.forEach { url ->
            whenever(tabAtelierClient.base(url))
                .thenReturn(TabAtelierBase.parse(url) ?: error("not a URL: $url"))
        }
    }

    /**
     * The save-from-editor path: a server that could not be reached, whose URL
     * the user then corrects and saves.
     *
     * Expected: the server is probed again and the stale error is gone, because
     * an error left on the row reads as the edit not having worked.
     */
    @Test
    fun savingAnEditedServer_reprobesIt_andClearsTheStaleError() = runTest {
        stubBase(DAEMON_URL, FIXED_URL)
        whenever(tabAtelierClient.fetchTabs(any(), any(), anyOrNull()))
            .thenThrow(IllegalStateException("connection refused"))
            .thenReturn(listOf(TabAtelierTab(id = "t1", name = "shell")))

        val host = tabAtelierHost()
        val viewModel = createViewModel()
        hostsFlow.value = listOf(host)
        advanceUntilIdle()
        assertTrue(
            "the failed probe should be on the row",
            viewModel.uiState.value.tabStates[host.id]?.error != null,
        )

        // Saving the corrected URL is an ordinary host write, and reaches this
        // ViewModel as the hosts Flow.
        hostsFlow.value = listOf(host.copy(tabAtelierUrl = FIXED_URL))
        advanceUntilIdle()

        val state = viewModel.uiState.value.tabStates[host.id]
        assertNull("the corrected server should not keep the old error", state?.error)
        assertEquals(1, state?.tabs?.size)
        verify(tabAtelierClient, times(2)).fetchTabs(any(), any(), anyOrNull())
    }

    /** A changed URL re-probes the server it now names. */
    @Test
    fun aChangedUrl_reprobesTheServer() = runTest {
        stubBase(DAEMON_URL, FIXED_URL)
        whenever(tabAtelierClient.fetchTabs(any(), any(), anyOrNull())).thenReturn(emptyList())

        val host = tabAtelierHost()
        createViewModel()
        hostsFlow.value = listOf(host)
        advanceUntilIdle()
        verify(tabAtelierClient, times(1)).fetchTabs(any(), any(), anyOrNull())

        hostsFlow.value = listOf(host.copy(tabAtelierUrl = FIXED_URL))
        advanceUntilIdle()
        verify(tabAtelierClient, times(2)).fetchTabs(any(), any(), anyOrNull())
    }

    /**
     * Adding a token re-probes: a daemon that refused the first probe because it
     * wants a token must be asked again once the editor has stored one. The
     * address is unchanged, so this is the token half of the fingerprint.
     */
    @Test
    fun storingAToken_reprobesTheServer() = runTest {
        stubBase(DAEMON_URL)
        whenever(tabAtelierClient.fetchTabs(any(), any(), anyOrNull())).thenReturn(emptyList())

        val host = tabAtelierHost()
        createViewModel()
        hostsFlow.value = listOf(host)
        advanceUntilIdle()
        verify(tabAtelierClient, times(1)).fetchTabs(any(), any(), anyOrNull())

        whenever(tabAtelierClient.hasToken(host.id)).thenReturn(true)
        // A distinct value, so the state flow emits; lastConnect is deliberately
        // not part of the fingerprint.
        hostsFlow.value = listOf(host.copy(lastConnect = 1L))
        advanceUntilIdle()
        verify(tabAtelierClient, times(2)).fetchTabs(any(), any(), anyOrNull())
    }

    /**
     * "Clear saved token" re-probes, and drops the error the unauthorised probe
     * left: that error describes a credential that is no longer stored.
     */
    @Test
    fun clearingTheToken_reprobesTheServer_andDropsTheUnauthorisedError() = runTest {
        stubBase(DAEMON_URL)
        whenever(tabAtelierClient.hasToken(7L)).thenReturn(true)
        whenever(tabAtelierClient.fetchTabs(any(), any(), anyOrNull()))
            .thenThrow(IllegalStateException("GET $DAEMON_URL/tabs returned HTTP 401"))
            .thenReturn(emptyList())

        val host = tabAtelierHost()
        val viewModel = createViewModel()
        hostsFlow.value = listOf(host)
        advanceUntilIdle()
        assertTrue(viewModel.uiState.value.tabStates[host.id]?.error != null)

        whenever(tabAtelierClient.hasToken(7L)).thenReturn(false)
        hostsFlow.value = listOf(host.copy(lastConnect = 1L))
        advanceUntilIdle()

        assertNull(
            "the error belonged to the token that was cleared",
            viewModel.uiState.value.tabStates[host.id]?.error,
        )
        verify(tabAtelierClient, times(2)).fetchTabs(any(), any(), anyOrNull())
    }

    /**
     * The trap the fingerprint exists for. ConnectBot writes `lastConnect` when
     * a session starts, so the hosts Flow emits while the user is doing
     * something else entirely; that write must not reach the daemon, or every
     * connect would re-probe every server.
     */
    @Test
    fun anUnrelatedHostWrite_doesNotReprobeTheServer() = runTest {
        stubBase(DAEMON_URL)
        whenever(tabAtelierClient.fetchTabs(any(), any(), anyOrNull())).thenReturn(emptyList())

        val host = tabAtelierHost()
        createViewModel()
        hostsFlow.value = listOf(host)
        advanceUntilIdle()

        hostsFlow.value = listOf(host.copy(lastConnect = System.currentTimeMillis()))
        advanceUntilIdle()

        verify(tabAtelierClient, times(1)).fetchTabs(any(), any(), anyOrNull())
    }

    private companion object {
        const val DAEMON_URL = "https://daemon.example"
        const val FIXED_URL = "https://fixed.example"
    }
}
