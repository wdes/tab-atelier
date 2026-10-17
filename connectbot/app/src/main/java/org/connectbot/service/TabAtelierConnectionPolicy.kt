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

package org.connectbot.service

import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import org.connectbot.tabatelier.TabAtelierAddresses
import org.connectbot.transport.TabAtelier
import timber.log.Timber

/**
 * Decides when a tab-atelier session should let go of its socket to save battery, and when
 * it should take it back.
 *
 * Only tab-atelier sessions are affected. SSH and Telnet sessions are left alone: this app
 * adds the tab-atelier protocol and these are its rules, and silently parking somebody
 * else's SSH session would be a change to behaviour upstream never agreed to.
 *
 * ## Why suspend and not disconnect
 *
 * The sessions are *suspended* — see `TabAtelier.suspendSession` — rather than closed. A
 * closed session is gone: the bridge is disposed, the Relay stops, and the user is back to
 * tapping the tab in the list to get a terminal at all. A suspended one keeps everything
 * except the socket, so coming back is one redial. That is what makes a ten-minute idle
 * timeout acceptable rather than infuriating: the cost of being idle is a reconnect, and
 * the cost of the reconnect is nothing, because the daemon holds the tab and replays its
 * scrollback.
 *
 * ## What counts as "no network"
 *
 * [ConnectivityManager]'s own answer, and specifically `NET_CAPABILITY_VALIDATED` on a
 * default network. Airplane mode needs no separate handling: it is one way to have no
 * network, and this reacts to the state rather than the cause, so Wi-Fi to a network with
 * no route out is treated exactly like airplane mode — which is the right call, because a
 * WebSocket cannot be kept alive over either.
 *
 * ## The pure part
 *
 * [shouldSuspend] is the whole rule as a function of three numbers, so the timing can be
 * tested without waiting ten real minutes or toggling a radio.
 */
class TabAtelierConnectionPolicy(
    private val scope: CoroutineScope,
    private val manager: TerminalManager,
    private val clock: () -> Long = { System.currentTimeMillis() },
) {
    /** When the app last saw the user do something. */
    @Volatile
    private var lastInteractionAt: Long = clock()

    /** Whether the phone has a default network it can actually route over. */
    @Volatile
    private var hasNetwork: Boolean = true

    private var ticker: kotlinx.coroutines.Job? = null
    private var networkCallback: ConnectivityManager.NetworkCallback? = null
    private var connectivity: ConnectivityManager? = null

    /**
     * Called whenever the user touches the app.
     *
     * Deliberately cheap and allocation-free: it is wired to `Activity.onUserInteraction`,
     * which runs on every touch, scroll and key press.
     */
    fun onUserInteraction() {
        lastInteractionAt = clock()
        // Resuming on interaction rather than on a timer is what makes the timeout feel
        // like nothing happened: the terminal is back by the time the finger lifts.
        resumeSuspended()
    }

    /** Whether [start] has been called. */
    val isStarted: Boolean get() = ticker?.isActive == true

    /** Starts the idle ticker and the network callbacks. */
    fun start(context: Context) {
        val app = context.applicationContext
        registerNetworkCallback(app)

        if (ticker?.isActive == true) return
        ticker = scope.launch {
            hasNetwork = readHasNetwork()
            connectivity?.let { updateLocalNetworkFlag(it) }
            while (isActive) {
                delay(TICK_MILLIS)
                if (shouldSuspend(now = clock(), lastInteractionAt = lastInteractionAt, hasNetwork = hasNetwork)) {
                    suspendAll()
                }
            }
        }
    }

    /** Stops the ticker and unregisters. */
    fun stop() {
        ticker?.cancel()
        ticker = null
        networkCallback?.let { callback ->
            runCatching { connectivity?.unregisterNetworkCallback(callback) }
        }
        networkCallback = null
        connectivity = null
    }

    private fun suspendAll() {
        for (bridge in manager.bridgesFlow.value) {
            val transport = bridge.tabAtelierTransport() ?: continue
            if (transport.isSuspended) continue
            Timber.i("Suspending tab-atelier session for %s: %s", bridge.host.nickname, reason())
            transport.suspendSession()
        }
        // The wake-up that gets it back has to come from somewhere, and on a phone that is
        // the next interaction — so the tick itself must not keep the CPU up. Leaving the
        // loop asleep for a full tick is enough; the session is already parked.
    }

    private fun resumeSuspended() {
        for (bridge in manager.bridgesFlow.value) {
            val transport = bridge.tabAtelierTransport() ?: continue
            if (!transport.isSuspended) continue
            Timber.i("Resuming tab-atelier session for %s", bridge.host.nickname)
            transport.resumeSession()
        }
    }

    /** For logging only; the rule itself is in [shouldSuspend]. */
    private fun reason(): String = if (!hasNetwork) "no network" else "idle"

    private fun registerNetworkCallback(context: Context) {
        if (networkCallback != null) return
        val cm = context.getSystemService(Context.CONNECTIVITY_SERVICE) as? ConnectivityManager ?: return
        connectivity = cm
        hasNetwork = readHasNetwork(cm)

        val callback = object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network: Network) {
                hasNetwork = readHasNetwork(cm)
                updateLocalNetworkFlag(cm)
                // **A new network is a different network.** Coming back on the same
                // Wi-Fi is a resume; coming back on cellular, or on a different LAN, is a
                // session whose address may no longer be the right one — so the resume
                // runs with the address ordering re-evaluated, which is what makes the
                // local-first rule work when the phone arrives home.
                if (hasNetwork) resumeSuspended()
            }

            override fun onLost(network: Network) {
                hasNetwork = readHasNetwork(cm)
                // Losing the only network is airplane mode, a dead Wi-Fi link, or a
                // captive portal: all the same thing here, and all a reason to let go
                // rather than let OkHttp rediscover it.
                if (!hasNetwork) suspendAll()
            }

            override fun onCapabilitiesChanged(network: Network, caps: NetworkCapabilities) {
                val validated = caps.hasCapability(NetworkCapabilities.NET_CAPABILITY_VALIDATED)
                if (!validated && hasNetwork) {
                    hasNetwork = false
                    suspendAll()
                } else if (validated && !hasNetwork) {
                    hasNetwork = true
                    updateLocalNetworkFlag(cm)
                    resumeSuspended()
                }
            }
        }
        networkCallback = callback
        runCatching { cm.registerDefaultNetworkCallback(callback) }
            .onFailure {
                // Not fatal: without callbacks the idle timer still works, and a dial with
                // no network fails and the retry loop handles it.
                Timber.w(it, "Could not observe the network; tab-atelier sessions will not suspend on link loss")
            }
    }

    private fun readHasNetwork(): Boolean {
        val cm = connectivity ?: return true
        return readHasNetwork(cm)
    }

    private fun readHasNetwork(cm: ConnectivityManager): Boolean {
        val network = cm.activeNetwork ?: return false
        val caps = cm.getNetworkCapabilities(network) ?: return false
        return caps.hasCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET) &&
            caps.hasCapability(NetworkCapabilities.NET_CAPABILITY_VALIDATED)
    }

    /**
     * Updates the flag the transport reads to decide whether a LAN address goes first.
     *
     * The signal is the phone's **own** address rather than the network's type: Wi-Fi at a
     * hotel is not the user's LAN, and cellular is never one, but a phone tethered to a
     * laptop on the LAN genuinely is. What both the good and bad cases share is whether the
     * local IPv4 address is private, so that is what is checked — reusing
     * [TabAtelierAddresses.isLocalAddress] so "private" means one thing in this app.
     *
     * A phone can have several addresses at once (Wi-Fi and a VPN, say), so *any* private
     * one counts: if the daemon is reachable on the LAN, having found that address is what
     * matters, and trying the LAN address first when it is not there costs one connect
     * timeout and falls through.
     */
    private fun updateLocalNetworkFlag(cm: ConnectivityManager) {
        val network = cm.activeNetwork ?: return
        val link = cm.getLinkProperties(network) ?: return
        val private = link.linkAddresses.any { address ->
            TabAtelierAddresses.isLocalAddress(address.address.hostAddress.orEmpty())
        }
        manager.isOnLocalNetwork = private
    }

    companion object {
        /**
         * How long a tab-atelier session may go untouched before it lets go of its socket.
         *
         * Ten minutes, which is what was asked for. Short enough that a phone left in a
         * pocket overnight is not holding a socket and waking for its keepalives; long
         * enough that putting the app down to read something does not cost a reconnect.
         */
        const val IDLE_TIMEOUT_MILLIS = 10 * 60 * 1000L

        /** How often the idle rule is evaluated. A minute is plenty for a ten-minute timer. */
        const val TICK_MILLIS = 60 * 1000L

        /**
         * The rule, as a function of what it depends on.
         *
         * No network always suspends, whatever the clock says — there is nothing to keep
         * alive and every attempt to keep it is a radio wake-up that fails. Otherwise
         * suspend once the app has gone [IDLE_TIMEOUT_MILLIS] without being touched.
         *
         * `>=` and not `>`: at exactly ten minutes the timeout has elapsed, and an
         * off-by-one here is invisible until somebody reads the code.
         */
        fun shouldSuspend(now: Long, lastInteractionAt: Long, hasNetwork: Boolean): Boolean {
            if (!hasNetwork) return true
            return now - lastInteractionAt >= IDLE_TIMEOUT_MILLIS
        }
    }
}

/**
 * The tab-atelier transport behind a bridge, or null when this is some other protocol.
 *
 * A helper rather than a cast at each site so that "only tab-atelier sessions are affected"
 * is stated once. [TerminalBridge.transport] is public, but its type is `AbsTransport`, and
 * every site would otherwise have to remember which subtype it is allowed to touch.
 */
internal fun TerminalBridge.tabAtelierTransport(): TabAtelier? = transport as? TabAtelier
