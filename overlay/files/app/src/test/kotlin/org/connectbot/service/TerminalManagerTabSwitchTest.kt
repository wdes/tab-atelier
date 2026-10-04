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
 * no tab-atelier server type, so there is nothing to preserve here.
 *
 * A tab-atelier server has many tabs, which is the case ConnectBot's session
 * model did not have: a session is keyed by host, so a second tab of a server
 * that already has a session used to be refused outright and the user kept seeing
 * the first tab. The manager now moves the existing session to the tab that was
 * tapped.
 *
 * Both tests stub `getConnectedBridge` to report a session that is already
 * connected. That is what makes them meaningful: with one connected, falling
 * through to `openConnection` would throw "Connection already open for that
 * nickname", so a returned session can only have come from the move.
 */
package org.connectbot.service

import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.test.StandardTestDispatcher
import kotlinx.coroutines.test.runTest
import org.connectbot.data.HostRepository
import org.connectbot.data.entity.Host
import org.connectbot.di.CoroutineDispatchers
import org.junit.Assert.assertSame
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.mockito.Mockito.doReturn
import org.mockito.Mockito.mock
import org.mockito.Mockito.spy
import org.mockito.Mockito.verify
import org.mockito.Mockito.`when`
import org.robolectric.RobolectricTestRunner

@OptIn(ExperimentalCoroutinesApi::class)
@RunWith(RobolectricTestRunner::class)
class TerminalManagerTabSwitchTest {
    private val dispatcher = StandardTestDispatcher()
    private val host = Host(id = 7L, nickname = "workstation")

    private lateinit var manager: TerminalManager
    private lateinit var repository: HostRepository
    private lateinit var bridge: TerminalBridge

    @Before
    fun setUp() {
        manager = spy(TerminalManager())
        manager.dispatchers = CoroutineDispatchers(default = dispatcher, io = dispatcher, main = dispatcher)
        repository = mock(HostRepository::class.java)
        manager.hostRepository = repository
        bridge = mock(TerminalBridge::class.java)
        `when`(bridge.host).thenReturn(host)
    }

    @Test
    fun anotherTabOfAConnectedServerMovesThatSession() = runTest(dispatcher) {
        `when`(repository.findHostById(host.id)).thenReturn(host)
        doReturn(bridge).`when`(manager).getConnectedBridge(host)
        doReturn(true).`when`(bridge).switchTab("tab-b")

        val result = manager.openConnectionForHostId(host.id, "tab-b")

        assertSame("the session on this server must be moved to the tab, not duplicated", bridge, result)
        verify(bridge).switchTab("tab-b")
    }

    /**
     * Tapping the tab a session is already on is a no-op, and must not become an
     * error. This is the case that falling through would break: `openConnection`
     * refuses a host that already has a session, so the user tapping the tab they
     * are already looking at would have been shown "Connection already open for
     * that nickname".
     */
    @Test
    fun tappingTheTabAlreadyOpenKeepsTheSession() = runTest(dispatcher) {
        `when`(repository.findHostById(host.id)).thenReturn(host)
        doReturn(bridge).`when`(manager).getConnectedBridge(host)
        // A move to the tab the session is already on takes nothing.
        doReturn(false).`when`(bridge).switchTab("tab-a")

        val result = manager.openConnectionForHostId(host.id, "tab-a")

        assertSame(bridge, result)
        verify(bridge).switchTab("tab-a")
    }

    /**
     * A move that failed still hands back the session, because the session has
     * reported the failure itself — it has no socket left and says so, the same as
     * any other lost connection. Returning null, or opening a second session, would
     * either hide the failure or duplicate it.
     */
    @Test
    fun aFailedMoveStillHandsBackTheSession() = runTest(dispatcher) {
        `when`(repository.findHostById(host.id)).thenReturn(host)
        doReturn(bridge).`when`(manager).getConnectedBridge(host)
        doReturn(false).`when`(bridge).switchTab("tab-c")

        assertSame(bridge, manager.openConnectionForHostId(host.id, "tab-c"))
    }
}
